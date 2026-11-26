//! The dereference loop of the read path (Go: pkg/pdfcpu/read.go:3829-4515 and :3090-3137;
//! docs/design.md section 15, WP 8): loading every object of the table, the linearization and
//! hint stream checks, the root version, and the encryption setup.
//!
//! Go's validation mode is always relaxed, `Reader15` is always set and the command is never one
//! that decrypts, encrypts or merges (docs/design.md section 6), so those branches of
//! `dereferenceXRefTable`, `validateInputVersion`, `handleUnencryptedFile` and `checkForEncryption`
//! are not ported. Go also has a second, sorted order of the dereference loop for the statistics
//! mode; the table is a `BTreeMap`, so one ascending loop serves both. The empty-password tail of
//! `setupEncryptionKey` is `crypto::authenticate`.

use super::objstm::{decode_object_streams, decompress_xref_table_entry};
use super::repair::bypass_xref_section;
use super::stream::load_stream_dict;
use super::xref::materialize_direct_info_dict;
use super::{
    ReadCx, bootstrap_integer_entry, dereferenced_dict, err_corrupt_xref_stream, parse_object_at,
    read_info,
};
use crate::crypto::{
    authenticate, classify_encryption_dictionary_error, supported_encryption,
    validate_encryption_version,
};
use crate::error::{Error, Sentinel};
use crate::model::deref::process_ref_counts_with_error;
use crate::model::{Context, EntryObject, Version, XRefTable};
use crate::object::string::{hex_decode, unescape};
use crate::object::{Array, Dict, Object};
use std::collections::BTreeMap;

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn nil_deref() -> Error {
    Error::Panic("nil pointer dereference")
}

// ---------------------------------------------------------------------------------------------
// Linearization

/// `validateLinearizationDirectEntries`: no entry of a linearization dict is a reference.
fn validate_linearization_direct_entries(d: &Dict, obj_nr: i64) -> Result<(), Error> {
    for (key, o) in d.iter() {
        if matches!(o, Object::Ref(_)) {
            return Err(Error::Corrupt(format!(
                "linearization dict obj#{obj_nr} entry {}: value must be direct",
                lossy(key)
            )));
        }
    }
    Ok(())
}

/// `handleLinearizationParmDict`: records the first linearization parameter dict and the offsets
/// of its hint tables.
fn handle_linearization_parm_dict(
    ctx: &mut Context,
    obj: &Object,
    obj_nr: i64,
) -> Result<(), Error> {
    if read_info(ctx)?.linearized {
        // Linearization dict already processed.
        return Ok(());
    }

    let Object::Dict(d) = obj else {
        return Ok(());
    };
    let Some(marker) = d.find(b"Linearized") else {
        return Ok(());
    };
    if matches!(marker, Object::Ref(_)) {
        return Err(Error::Corrupt(format!(
            "linearization dict obj#{obj_nr} entry Linearized: value must be direct"
        )));
    }
    if !d.is_linearization_parm_dict() {
        return Ok(());
    }
    validate_linearization_direct_entries(d, obj_nr)?;

    read_info(ctx)?.linearized = true;
    ctx.xref.linearization_objs.insert(obj_nr);

    let at = |what: &str| {
        Error::Corrupt(format!(
            "handleLinearizationParmDict: corrupt linearization dict at obj:{obj_nr} - {what}"
        ))
    };
    let Some(a) = d.array_entry(b"H") else {
        return Err(at("missing array entry H"));
    };
    if a.len() != 2 && a.len() != 4 {
        return Err(at("corrupt array entry H, needs length 2 or 4"));
    }
    let needs_integers = || at("corrupt array entry H, needs Integer values");

    let Some(Object::Integer(offset)) = a.first() else {
        return Err(needs_integers());
    };
    ctx.xref.offset_primary_hint_table = Some(*offset);

    if a.len() == 4 {
        let Some(Object::Integer(offset)) = a.get(2) else {
            return Err(needs_integers());
        };
        ctx.xref.offset_overflow_hint_table = Some(*offset);
    }
    Ok(())
}

/// `validateHintStreamObject`: no reference below a hint stream dictionary entry.
fn validate_hint_stream_object(
    x: &XRefTable,
    o: &Object,
    path: &str,
    depth: u64,
) -> Result<(), Error> {
    x.check_recursion_depth("hint stream dictionary", depth)?;
    match o {
        Object::Ref(_) => Err(Error::Corrupt(format!(
            "{path}: indirect reference not permitted"
        ))),
        Object::Array(a) => {
            for (i, e) in a.iter().enumerate() {
                validate_hint_stream_object(x, e, &format!("{path} array index {i}"), depth + 1)?;
            }
            Ok(())
        }
        Object::Dict(d) => {
            for (key, e) in d.iter() {
                let path = format!("{path} key {}", lossy(key));
                validate_hint_stream_object(x, e, &path, depth + 1)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `validateHintStreamDict`: the entries are checked in key order, so the first error is the one
/// of the lowest key.
fn validate_hint_stream_dict(x: &XRefTable, d: &Dict, obj_nr: i64) -> Result<(), Error> {
    for (key, o) in d.iter() {
        if key == b"Length" {
            continue;
        }
        let path = format!("hint stream obj#{obj_nr} entry {}", lossy(key));
        validate_hint_stream_object(x, o, &path, 0)?;
    }
    Ok(())
}

/// `validateHintStreamDictionaries`: the dictionaries of the hint streams of a linearized file.
/// If several objects have the offset, the lowest object number is the hint stream.
fn validate_hint_stream_dictionaries(ctx: &Context) -> Result<(), Error> {
    if !ctx.read.as_ref().is_some_and(|r| r.linearized) {
        return Ok(());
    }
    let offsets = [
        ctx.xref.offset_primary_hint_table,
        ctx.xref.offset_overflow_hint_table,
    ];
    for offset in offsets.into_iter().flatten() {
        let Some((obj_nr, entry)) = ctx
            .xref
            .table
            .iter()
            .find(|(_, e)| e.offset == Some(offset))
        else {
            continue;
        };
        if let Some(EntryObject::Obj(Object::Stream(sd))) = &entry.object {
            validate_hint_stream_dict(&ctx.xref, &sd.dict, *obj_nr)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Loading the objects

/// `unrecoverableObjectReadError`: an error that ends the read instead of skipping the object.
/// A cancel, a ledger refusal or a panic site is one as well: Go's context cancel is not
/// swallowed where it is raised, and there is no Go counterpart to the other two.
fn unrecoverable_object_read_error(e: &Error) -> bool {
    e.is(Sentinel::CorruptObjectOffset)
        || e.is(Sentinel::UnregisteredObject)
        || e.is(Sentinel::ObjectReadBudget)
        || e.is_limit_exceeded()
        || e.is(Sentinel::CircularRef)
        || matches!(
            e.root(),
            Error::Cancelled | Error::Alloc { .. } | Error::OutOfMemory | Error::Panic(_)
        )
}

/// `dereferenceAndLoad`: parses the object of entry `obj_nr` and, for a stream, loads its
/// content. An object that cannot be parsed is skipped (relaxed mode), unless the error is one
/// of [`unrecoverable_object_read_error`].
fn dereference_and_load(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    obj_nr: i64,
    offset: i64,
    generation: Option<i64>,
) -> Result<(), Error> {
    let generation = generation.ok_or_else(nil_deref)?;

    // Parse object from ctx: anything goes dict, array, integer, float, streamdict...
    let mut parsed = parse_object_at(rcx, ctx, offset, obj_nr, generation);
    if parsed.is_err() {
        let repair_offset = read_info(ctx)?.repair_offset;
        if repair_offset > 0 {
            parsed = parse_object_at(
                rcx,
                ctx,
                offset.wrapping_add(repair_offset),
                obj_nr,
                generation,
            );
        }
    }
    let o = match parsed {
        Ok(o) => o,
        Err(e) if unrecoverable_object_read_error(&e) => return Err(e),
        Err(_) => return Ok(()),
    };
    if o.is_null() {
        return Ok(());
    }

    // Linearization dicts are validated and recorded for stats only.
    handle_linearization_parm_dict(ctx, &o, obj_nr)?;

    // Handle stream dicts.
    let o = match o {
        Object::Stream(mut sd) => {
            load_stream_dict(rcx, ctx, &mut sd, obj_nr, generation, false)?;
            Object::Stream(sd)
        }
        other => other,
    };

    if let Some(e) = ctx.xref.entry_mut(obj_nr) {
        e.object = Some(EntryObject::Obj(o));
    }
    Ok(())
}

/// `dereferenceObjectEntry`: makes the object of entry `obj_nr` available: a compressed object
/// becomes a lazy one, an object in the input is parsed.
fn dereference_object(rcx: &mut ReadCx<'_>, ctx: &mut Context, obj_nr: i64) -> Result<(), Error> {
    let Some(entry) = ctx.xref.find(obj_nr) else {
        return Err(Error::Sentinel(Sentinel::CorruptXRefSubsection)
            .context(format!("xref entry for object {obj_nr} is nil")));
    };
    let (free, compressed, offset, generation, loaded) = (
        entry.free,
        entry.compressed,
        entry.offset,
        entry.generation,
        entry.object.is_some(),
    );

    if obj_nr > ctx.xref.max_obj_nr {
        ctx.xref.max_obj_nr = obj_nr;
    }

    if free {
        return Ok(());
    }

    if compressed {
        return decompress_xref_table_entry(&mut ctx.xref, obj_nr);
    }

    // Entry is in use. Already decompressed or used object w/o offset is ignored.
    let Some(offset) = offset.filter(|&o| o != 0) else {
        return Ok(());
    };

    if loaded {
        // Already dereferenced.
        return Ok(());
    }

    dereference_and_load(rcx, ctx, obj_nr, offset, generation)
}

/// `dereferenceObjectsAscending` and `dereferenceObjectsSorted`: loads every object in ascending
/// order of object number, then counts the references of the loaded objects.
fn dereference_objects_ascending(rcx: &mut ReadCx<'_>, ctx: &mut Context) -> Result<(), Error> {
    let _held = ctx
        .xref
        .mem()
        .charge((ctx.xref.table.len() as u64).saturating_mul(8))?;
    let mut keys: Vec<i64> = Vec::new();
    keys.try_reserve_exact(ctx.xref.table.len())?;
    keys.extend(ctx.xref.table.keys().copied());

    for &obj_nr in &keys {
        rcx.check()?;
        dereference_object(rcx, ctx, obj_nr)?;
    }

    for &obj_nr in &keys {
        let Some(entry) = ctx.xref.entry_mut(obj_nr) else {
            continue;
        };
        if entry.free || entry.compressed || !matches!(entry.object, Some(EntryObject::Obj(_))) {
            continue;
        }
        rcx.check()?;
        // The object is taken out of the table while its references are counted; the entry
        // stays, so a reference to itself still counts.
        let Some(EntryObject::Obj(o)) = entry.object.take() else {
            continue;
        };
        let r = process_ref_counts_with_error(&mut ctx.xref, &o);
        if let Some(entry) = ctx.xref.entry_mut(obj_nr) {
            entry.object = Some(EntryObject::Obj(o));
        }
        r?;
    }
    Ok(())
}

/// `dereferenceObjects`: loads all objects including the compressed ones. A corrupt offset or an
/// object that is not in the table makes one repair pass (`bypassXrefSection`) and a second try.
fn dereference_objects(rcx: &mut ReadCx<'_>, ctx: &mut Context) -> Result<(), Error> {
    if let Err(err) = dereference_objects_ascending(rcx, ctx) {
        let repair_offset = err.is(Sentinel::CorruptObjectOffset);
        let repair_missing = err.is(Sentinel::UnregisteredObject);
        if !repair_offset && !repair_missing {
            return Err(err);
        }
        bypass_xref_section(rcx, ctx, 0, &err, 1)?;
        dereference_objects_ascending(rcx, ctx)?;
    }
    validate_hint_stream_dictionaries(ctx)
}

// ---------------------------------------------------------------------------------------------
// The root version

/// `identifyRootVersion`: a `Version` entry of the catalog (since 1.4) is recorded as the root
/// version, as opposed to the header version. A value that is no version is ignored.
fn identify_root_version(x: &mut XRefTable) -> Result<(), Error> {
    let Some(s) = x
        .parse_root_version()
        .map_err(|e| e.context("root version"))?
    else {
        return Ok(());
    };
    let Ok(v) = Version::parse(&s).or_else(|_| Version::parse_relaxed(&s)) else {
        return Ok(());
    };
    x.root_version = Some(v);
    Ok(())
}

/// `validateObjectStreamIndexUniqueness` (7.5.7 and 7.5.8.3): no two compressed objects share an
/// object stream index.
fn validate_object_stream_index_uniqueness(ctx: &mut Context) -> Result<(), Error> {
    if read_info(ctx)?.object_streams.is_empty() {
        return Ok(());
    }
    let mut seen: BTreeMap<(i64, i64), i64> = BTreeMap::new();
    for (&obj_nr, entry) in &ctx.xref.table {
        if entry.free {
            continue;
        }
        let (Some(stm), Some(ind)) = (entry.object_stream, entry.object_stream_ind) else {
            continue;
        };
        if obj_nr == stm {
            // decodeObjectStream reports the more specific invalid object-stream entry.
            continue;
        }
        if let Some(prior) = seen.insert((stm, ind), obj_nr) {
            return Err(err_corrupt_xref_stream().context(format!(
                "compressed objects {prior} and {obj_nr} share object stream {stm} index {ind}"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Encryption

/// `materializeBootstrapIntegerEntry`: an indirect integer entry of `d` is loaded.
fn materialize_bootstrap_integer_entry(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    d: &Dict,
    key: &[u8],
) -> Result<(), Error> {
    if !matches!(d.find(key), Some(Object::Ref(_))) {
        return Ok(());
    }
    bootstrap_integer_entry(rcx, ctx, d, key).map(|_| ())
}

/// `materializeEncryptionIntegers`: the integers of the encryption dictionary, and of its crypt
/// filters, may be indirect. Object streams are not available yet, so they are loaded here.
fn materialize_encryption_integers(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    d: &Dict,
) -> Result<(), Error> {
    for key in [b"V".as_slice(), b"Length", b"R", b"P"] {
        materialize_bootstrap_integer_entry(rcx, ctx, d, key)
            .map_err(|e| e.context("encrypt dict"))?;
    }

    let Some(cf) = d.dict_entry(b"CF") else {
        return Ok(());
    };
    for (name, o) in cf.iter() {
        let Object::Dict(cd) = o else {
            continue;
        };
        materialize_bootstrap_integer_entry(rcx, ctx, cd, b"Length")
            .map_err(|e| e.context(format!("crypt filter {}", lossy(name))))?;
    }
    Ok(())
}

/// `IDFirstElement`: the first element of the trailer `ID`.
fn id_first_element(id: &Array) -> Result<Vec<u8>, Error> {
    match id.first() {
        Some(Object::Hex(h)) => hex_decode(h),
        Some(Object::String(s)) => Ok(unescape(s)),
        _ => Err(Error::Corrupt(
            "id must contain hex literals or string literals".to_string(),
        )),
    }
}

/// `setupEncryptionKey` for empty passwords: validates the encryption dictionary `d`, reads the
/// file `ID`, and derives the file key from the empty owner or user password.
fn setup_encryption_key(ctx: &mut Context, d: &Dict) -> Result<(), Error> {
    let mut e = supported_encryption(&mut ctx.xref, d)
        .map_err(|e| e.context("parse encryption dictionary"))?;

    if let Some(id) = ctx.xref.id.as_ref().filter(|a| !a.is_empty()) {
        e.id = id_first_element(id).map_err(|err| err.context("read encryption ID"))?;
    }

    let res = authenticate(&e, &mut ctx.xref.enc_key);
    ctx.xref.e = Some(e);
    res
}

/// An error of the dereference of the encryption dictionary: a missing, mistyped or unresolvable
/// dictionary is a malformed one; any other error (a limit, the cancel flag) is not.
fn classify_encryption_dict_error(err: Error) -> Error {
    let err = classify_encryption_dictionary_error(err);
    // `errCorruptDictObject`.
    if matches!(err.root(), Error::Corrupt(m) if m == "corrupt dict object") {
        return Error::MalformedEncryption(Box::new(err));
    }
    err
}

/// `checkForEncryption`: for an encrypted file, validates the encryption dictionary and sets up
/// the file key, so that the strings and streams read from now on are decrypted. Files are
/// opened with the empty password only.
fn check_for_encryption(rcx: &mut ReadCx<'_>, ctx: &mut Context) -> Result<(), Error> {
    let Some(ind_ref) = ctx.xref.encrypt else {
        // This file is not encrypted.
        return Ok(());
    };

    // Dereference encryptDict.
    let obj_nr = ind_ref.num;
    let d = dereferenced_dict(rcx, ctx, obj_nr).map_err(|e| {
        classify_encryption_dict_error(e).context(format!("encryption dictionary obj#{obj_nr}"))
    })?;

    materialize_encryption_integers(rcx, ctx, &d)
        .map_err(|e| e.context(format!("encryption dictionary obj#{obj_nr}")))?;

    // We need to decrypt this file in order to read it.
    setup_encryption_key(ctx, &d).map_err(|e| e.context("encryption setup"))
}

// ---------------------------------------------------------------------------------------------
// The loop

/// `dereferenceXRefTable` (read.go:4311): loads every object of the table, decodes the object
/// streams, identifies the root version and sets up decryption.
///
/// # Errors
/// The encryption errors ([`Error::WrongPassword`], [`Error::MalformedEncryption`],
/// [`Error::UnsupportedEncryption`]), the errors of the object stream and object loads that end a
/// read, [`Error::Cancelled`].
pub fn dereference_xref_table(rcx: &mut ReadCx<'_>, ctx: &mut Context) -> Result<(), Error> {
    validate_object_stream_index_uniqueness(ctx)?;

    // Note for encrypted files: the empty password is tried, owner first, then user. Access may
    // be restricted (Decode access privileges).
    check_for_encryption(rcx, ctx)?;

    // Prepare decompressed objects.
    decode_object_streams(rcx, ctx)?;

    // For each xRefTableEntry assign a Object either by parsing from file or pointing to a
    // decompressed object.
    dereference_objects(rcx, ctx)?;

    // Identify an optional Version entry in the root object/catalog.
    identify_root_version(&mut ctx.xref)?;
    validate_encryption_version(&mut ctx.xref, rcx.cancel)?;
    materialize_direct_info_dict(&mut ctx.xref)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::Entry;
    use crate::object::{Ref, StreamDict};
    use crate::read::read;
    use std::sync::atomic::AtomicBool;

    fn ctx_for(input: &[u8]) -> Context {
        match Context::new(input.len() as u64, Config::default()) {
            Ok(c) => c,
            Err(e) => panic!("context: {e}"),
        }
    }

    fn r(n: i64) -> Object {
        Object::Ref(Ref {
            num: n,
            generation: 0,
        })
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    // Port of TestValidateHintStreamDictSelectsLowestKey.
    #[test]
    fn hint_stream_dict_selects_lowest_key() {
        let ctx = ctx_for(b"");
        let d = dict(&[("Zulu", r(9)), ("Alpha", r(3))]);
        let err = validate_hint_stream_dict(&ctx.xref, &d, 1).unwrap_err();
        assert!(err.to_string().contains("entry Alpha"), "{err}");
    }

    // Port of TestValidateHintStreamNestedDictSelectsLowestKey.
    #[test]
    fn hint_stream_nested_dict_selects_lowest_key() {
        let ctx = ctx_for(b"");
        let d = dict(&[(
            "Container",
            Object::Dict(dict(&[("Zulu", r(9)), ("Alpha", r(3))])),
        )]);
        let err = validate_hint_stream_dict(&ctx.xref, &d, 1).unwrap_err();
        assert!(
            err.to_string().contains("entry Container key Alpha"),
            "{err}"
        );
    }

    // Port of TestValidateHintStreamDictionariesSelectsLowestObject.
    #[test]
    fn hint_stream_dictionaries_select_lowest_object() {
        let mut ctx = ctx_for(b"");
        if let Some(r) = ctx.read.as_mut() {
            r.linearized = true;
        }
        ctx.xref.offset_primary_hint_table = Some(100);
        for obj_nr in [9, 3] {
            let sd = StreamDict {
                dict: dict(&[("Alpha", r(obj_nr))]),
                ..StreamDict::default()
            };
            let mut e = Entry::gen0(Object::Stream(Box::new(sd)));
            e.offset = Some(100);
            ctx.xref.table.insert(obj_nr, e);
        }
        let err = validate_hint_stream_dictionaries(&ctx).unwrap_err();
        assert!(err.to_string().contains("hint stream obj#3"), "{err}");

        // A direct value, the Length, and an array or dict that holds one.
        let ok = dict(&[
            ("Length", r(1)),
            ("A", Object::Array(vec![Object::Integer(1)])),
        ]);
        validate_hint_stream_dict(&ctx.xref, &ok, 1).unwrap();
        let bad = dict(&[("A", Object::Array(vec![Object::Integer(1), r(2)]))]);
        let err = validate_hint_stream_dict(&ctx.xref, &bad, 1).unwrap_err();
        assert!(err.to_string().contains("entry A array index 1"), "{err}");
    }

    #[test]
    fn linearization_parm_dict() {
        let mut ctx = ctx_for(b"");
        let lin = |h: Object| {
            Object::Dict(dict(&[
                ("Linearized", Object::Integer(1)),
                ("H", h),
                ("L", Object::Integer(100)),
            ]))
        };

        // Not a linearization dict.
        handle_linearization_parm_dict(&mut ctx, &Object::Dict(Dict::default()), 5).unwrap();
        assert!(!ctx.read.as_ref().is_some_and(|r| r.linearized));

        // Corrupt hint offsets.
        for h in [
            Object::Integer(1),
            Object::Array(vec![Object::Integer(1)]),
            Object::Array(vec![Object::Name(b"x".to_vec()), Object::Integer(2)]),
        ] {
            let mut ctx = ctx_for(b"");
            assert!(handle_linearization_parm_dict(&mut ctx, &lin(h), 5).is_err());
        }
        // A reference among the entries.
        let mut bad = dict(&[("Linearized", Object::Integer(1)), ("L", r(3))]);
        bad.set(
            b"H",
            Object::Array(vec![Object::Integer(1), Object::Integer(2)]),
        );
        let mut c2 = ctx_for(b"");
        let err = handle_linearization_parm_dict(&mut c2, &Object::Dict(bad), 5).unwrap_err();
        assert!(err.to_string().contains("value must be direct"), "{err}");

        let two = Object::Array(vec![Object::Integer(10), Object::Integer(20)]);
        handle_linearization_parm_dict(&mut ctx, &lin(two), 5).unwrap();
        assert!(ctx.read.as_ref().is_some_and(|r| r.linearized));
        assert!(ctx.xref.linearization_objs.contains(&5));
        assert_eq!(ctx.xref.offset_primary_hint_table, Some(10));
        assert_eq!(ctx.xref.offset_overflow_hint_table, None);

        // Only the first one counts.
        let four = Object::Array(vec![
            Object::Integer(1),
            Object::Integer(2),
            Object::Integer(3),
            Object::Integer(4),
        ]);
        handle_linearization_parm_dict(&mut ctx, &lin(four.clone()), 6).unwrap();
        assert_eq!(ctx.xref.offset_primary_hint_table, Some(10));
        let mut c3 = ctx_for(b"");
        handle_linearization_parm_dict(&mut c3, &lin(four), 6).unwrap();
        assert_eq!(c3.xref.offset_overflow_hint_table, Some(3));
    }

    #[test]
    fn unrecoverable_errors() {
        for e in [
            Error::Sentinel(Sentinel::CorruptObjectOffset),
            Error::Sentinel(Sentinel::UnregisteredObject).context("x"),
            Error::Sentinel(Sentinel::ObjectReadBudget),
            Error::Limit(crate::LimitKind::Parsed),
            Error::Sentinel(Sentinel::CircularRef),
            Error::Cancelled,
            Error::OutOfMemory,
        ] {
            assert!(unrecoverable_object_read_error(&e), "{e}");
        }
        for e in [
            Error::Corrupt("x".to_string()),
            Error::Sentinel(Sentinel::Eof),
            Error::DecodeLimit,
        ] {
            assert!(!unrecoverable_object_read_error(&e), "{e}");
        }
    }

    #[test]
    fn root_version_is_identified() {
        let mut ctx = ctx_for(b"");
        ctx.xref.header_version = Some(Version::V14);
        ctx.xref.root = Some(Ref {
            num: 1,
            generation: 0,
        });
        let cat = |v: &str| {
            Entry::gen0(Object::Dict(dict(&[(
                "Version",
                Object::Name(v.as_bytes().to_vec()),
            )])))
        };
        ctx.xref.table.insert(1, cat("1.7"));
        identify_root_version(&mut ctx.xref).unwrap();
        assert_eq!(ctx.xref.root_version, Some(Version::V17));
        assert_eq!(ctx.xref.version(), Ok(Version::V17));

        // The relaxed spelling, and a value that is no version.
        ctx.xref.root_version = None;
        ctx.xref.table.insert(1, cat("1.7.0"));
        identify_root_version(&mut ctx.xref).unwrap();
        assert_eq!(ctx.xref.root_version, Some(Version::V17));
        ctx.xref.root_version = None;
        ctx.xref.table.insert(1, cat("9.9"));
        identify_root_version(&mut ctx.xref).unwrap();
        assert_eq!(ctx.xref.root_version, None);
    }

    #[test]
    fn object_stream_index_uniqueness() {
        let mut ctx = ctx_for(b"");
        let comp = |stm: i64, ind: i64| Entry {
            compressed: true,
            object_stream: Some(stm),
            object_stream_ind: Some(ind),
            ..Entry::default()
        };
        // Without object streams there is nothing to check.
        ctx.xref.table.insert(3, comp(9, 0));
        ctx.xref.table.insert(4, comp(9, 0));
        validate_object_stream_index_uniqueness(&mut ctx).unwrap();

        if let Some(r) = ctx.read.as_mut() {
            r.object_streams.insert(9);
        }
        let err = validate_object_stream_index_uniqueness(&mut ctx).unwrap_err();
        assert!(
            err.to_string()
                .contains("compressed objects 3 and 4 share object stream 9 index 0"),
            "{err}"
        );
        assert_eq!(err.root(), &err_corrupt_xref_stream());

        ctx.xref.table.insert(4, comp(9, 1));
        validate_object_stream_index_uniqueness(&mut ctx).unwrap();
    }

    /// A classic PDF of the object bodies 1..=n and a trailer with `extra`.
    fn classic_pdf(bodies: &[&str], extra: &str) -> Vec<u8> {
        let mut f = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in bodies.iter().enumerate() {
            offsets.push(f.len());
            f.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = f.len();
        f.extend_from_slice(format!("xref\n0 {}\n", bodies.len() + 1).as_bytes());
        f.extend_from_slice(b"0000000000 65535 f \n");
        for o in offsets {
            f.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        f.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R {extra} >>\nstartxref\n{xref}\n%%EOF\n",
                bodies.len() + 1
            )
            .as_bytes(),
        );
        f
    }

    #[test]
    fn read_loads_objects_and_counts_references() {
        let data = classic_pdf(
            &[
                "<< /Type /Catalog /Pages 2 0 R /Version /1.7 >>",
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
                "<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>",
                "<< /Length 5 >>\nstream\nhello\nendstream",
                "42",
            ],
            "",
        );
        let cancel = AtomicBool::new(false);
        let ctx = read(&data, &Config::default(), &cancel).unwrap();
        let x = &ctx.xref;
        assert_eq!(x.size, Some(6));
        assert_eq!(x.max_obj_nr, 5);
        assert_eq!(x.root_version, Some(Version::V17));
        for n in 1..=5 {
            assert!(
                x.find(n).is_some_and(|e| e.object.is_some()),
                "object {n} is not loaded"
            );
        }
        match x.find(4).and_then(|e| e.object.as_ref()) {
            Some(EntryObject::Obj(Object::Stream(sd))) => {
                assert_eq!(sd.raw.as_deref(), Some(b"hello".as_slice()));
                assert_eq!(sd.stream_length, Some(5));
                assert!(sd.content.is_none());
            }
            other => panic!("object 4: {other:?}"),
        }
        assert!(matches!(
            x.find(5).and_then(|e| e.object.as_ref()),
            Some(EntryObject::Obj(Object::Integer(42)))
        ));
        // Catalog -> Pages, Pages -> Page, Page -> Parent and Contents.
        assert_eq!(x.find(2).map(|e| e.ref_count), Some(2));
        assert_eq!(x.find(3).map(|e| e.ref_count), Some(1));
        assert_eq!(x.find(4).map(|e| e.ref_count), Some(1));
    }

    const R2_O: &str = "c92422687facee686e373f10b5c7d04738053152f7e2ee30e11c69ec442576ab";
    const R2_U: &str = "4499a4996ed25de042be3e1e8b8a44ce931bc9a5b6248bdf6af1020eaf0393e3";
    const R2_ID: &str = "af5a0bc2e84136018337bb9602334dd9";

    fn r2_pdf(u: &str) -> Vec<u8> {
        classic_pdf(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [] /Count 0 >>",
                &format!("<< /Filter /Standard /V 1 /R 2 /O <{R2_O}> /U <{u}> /P -4 >>"),
            ],
            &format!("/Encrypt 3 0 R /ID [<{R2_ID}> <{R2_ID}>]"),
        )
    }

    // The dictionary and the keys are from qpdf (see crypto/auth.rs): the empty user password
    // opens the file.
    #[test]
    fn read_opens_an_encrypted_file_with_the_empty_password() {
        let cancel = AtomicBool::new(false);
        let ctx = read(&r2_pdf(R2_U), &Config::default(), &cancel).unwrap();
        let x = &ctx.xref;
        assert_eq!(x.enc_key.len(), 5);
        let e = x.e.as_ref().unwrap();
        assert_eq!((e.v, e.r, e.p), (1, 2, -4));
        assert_eq!(
            e.id,
            crate::object::string::hex_decode(R2_ID.as_bytes()).unwrap()
        );
        assert_eq!(
            x.encrypt,
            Some(Ref {
                num: 3,
                generation: 0
            })
        );
    }

    #[test]
    fn read_rejects_a_wrong_password() {
        let wrong = format!("{}{}", &R2_U[..R2_U.len() - 2], "00");
        let cancel = AtomicBool::new(false);
        let err = read(&r2_pdf(&wrong), &Config::default(), &cancel).unwrap_err();
        assert!(err.is_wrong_password(), "{err}");
    }

    #[test]
    fn read_rejects_a_missing_encryption_dictionary() {
        let data = classic_pdf(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [] /Count 0 >>",
                "42",
            ],
            &format!("/Encrypt 3 0 R /ID [<{R2_ID}> <{R2_ID}>]"),
        );
        let cancel = AtomicBool::new(false);
        let err = read(&data, &Config::default(), &cancel).unwrap_err();
        assert_eq!(err.class(), "encryption/malformed", "{err}");
        assert!(
            err.to_string().contains("encryption dictionary obj#3"),
            "{err}"
        );
    }

    fn invalid_object_stream_entry_pdf(free: bool) -> Vec<u8> {
        let mut b = b"%PDF-1.5\n".to_vec();
        let mut offsets = [0usize; 7];
        for (i, body) in [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << >> >>",
        ]
        .iter()
        .enumerate()
        {
            offsets[i + 1] = b.len();
            b.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        offsets[6] = b.len();
        // Object stream 5 incorrectly claims to live inside itself.
        let stream_entry = if free { [0, 0, 0] } else { [2, 5, 0] };
        let mut data = Vec::new();
        for row in [
            [0, 0, 65535],
            [1, offsets[1], 0],
            [1, offsets[2], 0],
            [1, offsets[3], 0],
            [2, 5, 0],
            stream_entry,
            [1, offsets[6], 0],
        ] {
            data.push(row[0] as u8);
            data.extend_from_slice(&(row[1] as u32).to_be_bytes());
            data.extend_from_slice(&(row[2] as u16).to_be_bytes());
        }
        b.extend_from_slice(
            format!(
                "6 0 obj\n<< /Type /XRef /Size 7 /W [1 4 2] /Root 1 0 R /Length {} >>\nstream\n",
                data.len()
            )
            .as_bytes(),
        );
        b.extend_from_slice(&data);
        b.extend_from_slice(
            format!("\nendstream\nendobj\nstartxref\n{}\n%%EOF\n", offsets[6]).as_bytes(),
        );
        b
    }

    // Port of TestReadRejectsInvalidObjectStreamEntries.
    #[test]
    fn read_rejects_invalid_object_stream_entries() {
        for free in [false, true] {
            let cancel = AtomicBool::new(false);
            let err = read(
                &invalid_object_stream_entry_pdf(free),
                &Config::default(),
                &cancel,
            )
            .unwrap_err();
            assert_eq!(
                err.root(),
                &crate::read::objstm::err_missing_object_stream_entry(),
                "free={free}: {err}"
            );
            assert!(err.to_string().contains("object stream obj#5"), "{err}");
        }
    }
}
