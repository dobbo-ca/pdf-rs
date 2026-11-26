//! The object stream half of the read path (Go: pkg/pdfcpu/read.go:546-830 and :3554-3830;
//! docs/design.md section 15, WP 8): the prolog of an object stream, the lazy table entries of its
//! objects, and the decoding and validation of every object stream of a file.
//!
//! The full decode of an object stream and the parse of one of its objects happen lazily, once
//! per stream, in `XRefTable::decode_lazy_entry` (model/deref.rs); `compressedObject` is
//! `model::deref::compressed_object`. Go's `ObjectStreamDict.ObjArray` holds one lazy object per
//! entry; here `ObjectStreamDict::obj_array` holds the absolute offset of each entry as an
//! `Object::Integer`, and entry `i` is the range from offset `i` to offset `i + 1` (or the end of
//! the content for the last one), which is what [`decompress_xref_table_entry`] turns into an
//! `EntryObject::Lazy`.

use super::stream::{load_encoded_stream_content, save_decoded_stream_content};
use super::{
    ReadCx, atoi, bootstrap_integer_entry, dereferenced_object, go_fields, ix, lim,
    parse_object_at, read_info, to_i64,
};
use crate::error::Error;
use crate::ledger::Bytes;
use crate::model::{Context, EntryObject, XRefTable};
use crate::object::{Array, LazyRef, Object, ObjectStreamDict, StreamDict};
use crate::parse::object_stream_dict_with_resolved_integers;
use std::borrow::Cow;
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------------------------
// Errors

/// `errMissingObjectStreamEntry`.
pub fn err_missing_object_stream_entry() -> Error {
    Error::Corrupt("missing object stream xref entry".to_string())
}

/// `errMissingObjectStream`.
pub fn err_missing_object_stream() -> Error {
    Error::Corrupt("missing object stream".to_string())
}

/// `errMissingObjectStreamObjects`.
pub fn err_missing_object_stream_objects() -> Error {
    Error::Corrupt("missing object stream objects".to_string())
}

/// `errCorruptObjectStream`.
pub fn err_corrupt_object_stream() -> Error {
    Error::Corrupt("corrupt object stream".to_string())
}

/// `errCorruptObjectStreamDict`.
pub fn err_corrupt_object_stream_dict() -> Error {
    Error::Corrupt("corrupt object stream dict".to_string())
}

/// `errObjectStreamObjectCountLimit` (it does not wrap `ErrLimitExceeded` in Go).
pub fn err_object_stream_object_count_limit() -> Error {
    Error::Corrupt("object stream object count limit exceeded".to_string())
}

// ---------------------------------------------------------------------------------------------
// The prolog

/// `buildObjectArrayForObjectStream`: the absolute offset of each entry of the prolog `objs`
/// (object number, offset, object number, offset, ...).
fn build_object_array(
    rcx: &ReadCx<'_>,
    first_obj_offset: i64,
    objs: &[&[u8]],
    full_content: bool,
    content_len: i64,
) -> Result<Array, Error> {
    let mut offsets: Array = Vec::new();
    offsets.try_reserve_exact(objs.len() / 2)?;
    let mut offset_old = 0i64;

    for (k, pair) in objs.chunks_exact(2).enumerate() {
        rcx.check()?;

        let [_, offset_str] = pair else {
            break;
        };
        let offset =
            atoi(offset_str).map_err(|e| e.context(format!("object stream dict offset {k}")))?;

        let offset = offset.wrapping_add(first_obj_offset);
        if offset < first_obj_offset {
            return Err(err_corrupt_object_stream_dict());
        }
        if full_content && offset > content_len {
            return Err(err_corrupt_object_stream_dict());
        }
        if k > 0 && offset < offset_old {
            return Err(err_corrupt_object_stream_dict());
        }

        offsets.push(Object::Integer(offset));
        offset_old = offset;
    }

    Ok(offsets)
}

/// `parseObjectStreamWithBudget`: reads the prolog of `osd` and sets `osd.obj_array`. Only the
/// prolog is decoded here, unless the content is there already (a sole Crypt filter, or an empty
/// stream); the objects are decoded lazily.
///
/// # Errors
/// The decode errors of the prolog, [`err_corrupt_object_stream_dict`], an entry count over
/// `MaxObjectStreamCount` ([`err_object_stream_object_count_limit`]), [`Error::Cancelled`].
pub fn parse_object_stream(
    rcx: &ReadCx<'_>,
    ctx: &Context,
    osd: &mut ObjectStreamDict,
) -> Result<(), Error> {
    let limits = &ctx.xref.conf.limits;
    let mem = ctx.xref.mem();

    let (decoded, full_content): (Bytes, bool) = match osd.stream.content.clone() {
        Some(c) => (c, true),
        None => {
            // The actual content will be decoded lazily, only decode the prolog here.
            let prolog = osd
                .stream
                .decode_length_with_limit(
                    osd.first_obj_offset,
                    lim(limits.max_decode_bytes),
                    mem,
                    rcx.cancel,
                )
                .map_err(|e| e.context("decode object stream prolog"))?;
            (prolog, false)
        }
    };
    let content_len = to_i64(decoded.len());
    if osd.first_obj_offset > content_len {
        return Err(err_corrupt_object_stream_dict());
    }
    let prolog = decoded.get(..ix(osd.first_obj_offset)).unwrap_or(&[]);

    // Remove inline comment.
    let prolog = match prolog.iter().position(|&b| b == b'%') {
        Some(i) => prolog.get(..i).unwrap_or(&[]),
        None => prolog,
    };

    // The separator used in the prolog shall be white space but some PDF writers use 0x00.
    let prolog: Cow<'_, [u8]> = if prolog.contains(&0) {
        let mut v: Vec<u8> = Vec::new();
        v.try_reserve_exact(prolog.len())?;
        v.extend(prolog.iter().map(|&b| if b == 0 { b' ' } else { b }));
        Cow::Owned(v)
    } else {
        Cow::Borrowed(prolog)
    };

    let objs = go_fields(&prolog)?;
    if objs.len() % 2 > 0 {
        return Err(err_corrupt_object_stream_dict());
    }
    let entry_count = to_i64(objs.len() / 2);
    let max_count = lim(limits.max_object_stream_count);
    if entry_count > max_count {
        return Err(err_object_stream_object_count_limit().context(format!(
            "object stream object count {entry_count} exceeds limit {max_count}"
        )));
    }
    if entry_count != osd.obj_count {
        return Err(err_corrupt_object_stream_dict().context(format!(
            "object stream index contains {entry_count} entries, expected {}",
            osd.obj_count
        )));
    }

    // e.g., 10 0 11 25 = 2 Objects: #10 @ offset 0, #11 @ offset 25
    osd.obj_array = build_object_array(rcx, osd.first_obj_offset, &objs, full_content, content_len)
        .map_err(|e| e.context("build object stream object array"))?;
    Ok(())
}

/// `LazyObjectStreamObject` of entry `index` of the object stream `stm`.
fn lazy_ref(osd: &ObjectStreamDict, stm: i64, index: i64) -> Result<LazyRef, Error> {
    let Object::Integer(start) = osd.indexed_object(index)? else {
        return Err(err_corrupt_object_stream_dict());
    };
    let next = usize::try_from(index)
        .ok()
        .and_then(|i| i.checked_add(1))
        .and_then(|i| osd.obj_array.get(i));
    let end = match next {
        Some(Object::Integer(e)) => *e,
        _ => -1,
    };
    Ok(LazyRef {
        stm,
        start: *start,
        end,
    })
}

/// `decompressXRefTableEntry` (read.go:3554): turns the compressed entry `obj_nr` into an
/// uncompressed one holding its object of the object stream, which is lazy: it is parsed when
/// the object is first used.
///
/// # Errors
/// [`err_missing_object_stream_entry`] and [`err_missing_object_stream`] for an object stream that
/// is not there, an index that is not in the prolog.
pub fn decompress_xref_table_entry(x: &mut XRefTable, obj_nr: i64) -> Result<(), Error> {
    let nil = || Error::Panic("nil pointer dereference");
    let entry = x.find(obj_nr).ok_or_else(nil)?;
    let (Some(stm), Some(ind)) = (entry.object_stream, entry.object_stream_ind) else {
        return Err(nil());
    };

    // Resolve xRefTable entry of referenced object stream.
    let Some(stm_entry) = x.find(stm) else {
        return Err(err_missing_object_stream_entry().context(format!("object stream {stm}")));
    };

    // Object of this entry has to be a ObjectStreamDict.
    let Some(EntryObject::ObjStm(osd)) = &stm_entry.object else {
        return Err(err_missing_object_stream().context(format!("object stream {stm}")));
    };

    // Get indexed object from ObjectStreamDict.
    let lazy = lazy_ref(osd, stm, ind).map_err(|e| {
        e.context(format!(
            "decompressXRefTableEntry: problem dereferencing object stream {stm}"
        ))
    })?;

    // Save object to XRefRableEntry.
    if let Some(e) = x.entry_mut(obj_nr) {
        e.object = Some(EntryObject::Lazy(lazy));
        e.generation = Some(0);
        e.compressed = false;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Decoding the object streams

/// `decodeObjectStreamObjects`: the object stream dict of the stream dict `sd` of object `obj_nr`
/// with its prolog parsed.
fn decode_object_stream_objects(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    sd: &StreamDict,
    obj_nr: i64,
) -> Result<ObjectStreamDict, Error> {
    let parse_dict = |key: &str, e: Error| {
        e.context(format!(
            "object stream obj#{obj_nr}: parse dictionary: object stream entry {key}"
        ))
    };
    let (first, _) = bootstrap_integer_entry(rcx, ctx, &sd.dict, b"First")
        .map_err(|e| parse_dict("First", e))?;
    let (n, _) =
        bootstrap_integer_entry(rcx, ctx, &sd.dict, b"N").map_err(|e| parse_dict("N", e))?;
    let mut osd = object_stream_dict_with_resolved_integers(sd, &ctx.xref.conf.limits, n, first)
        .map_err(|e| e.context(format!("object stream obj#{obj_nr}: parse dictionary")))?;
    osd.max_decode_bytes = lim(ctx.xref.conf.limits.decode_limit());
    ctx.xref
        .charge_object_stream_entries(osd.obj_count)
        .map_err(|e| e.context(format!("object stream obj#{obj_nr}")))?;

    // Parse the prolog of this object stream and save the entries to ObjectStreamDict.ObjArray.
    parse_object_stream(rcx, ctx, &mut osd)
        .map_err(|e| e.context(format!("object stream obj#{obj_nr}: parse objects")))?;

    if osd.obj_array.is_empty() {
        return Err(
            err_missing_object_stream_objects().context(format!("object stream obj#{obj_nr}"))
        );
    }
    Ok(osd)
}

/// `decodeObjectStream`: parses the object stream `obj_nr` from the input and saves its object
/// stream dict to the table entry. The content is decoded lazily.
///
/// # Errors
/// [`err_missing_object_stream_entry`] for an entry that is not an uncompressed in-use object
/// (stream objects cannot reside in object streams, 7.5.7), [`err_corrupt_object_stream`] for an
/// object that is no stream, the errors of the parse, the content load and the prolog.
pub fn decode_object_stream(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    obj_nr: i64,
) -> Result<(), Error> {
    // Stream objects cannot reside in object streams (7.5.7).
    let missing =
        || err_missing_object_stream_entry().context(format!("object stream obj#{obj_nr}"));
    let (offset, generation) = match ctx.xref.find(obj_nr) {
        Some(e) if !e.free && !e.compressed => match (e.offset, e.generation) {
            (Some(o), Some(g)) => (o, g),
            _ => return Err(missing()),
        },
        _ => return Err(missing()),
    };

    // Parse object stream from file.
    let o = parse_object_at(rcx, ctx, offset, obj_nr, generation)
        .map_err(|e| e.context(format!("object stream obj#{obj_nr}: parse object")))?;

    // Ensure StreamDict.
    let Object::Stream(mut sd) = o else {
        return Err(err_corrupt_object_stream().context(format!("object stream obj#{obj_nr}")));
    };

    // Load encoded stream content to xRefTable.
    load_encoded_stream_content(rcx, ctx, &mut sd, false)
        .map_err(|e| e.context(format!("object stream obj#{obj_nr}: load encoded content")))?;

    // Will only decrypt, the actual stream content is decoded later lazily.
    save_decoded_stream_content(rcx, ctx, &mut sd, obj_nr, generation, false)
        .map_err(|e| e.context(format!("object stream obj#{obj_nr}: decode content")))?;

    // We have an object stream.
    read_info(ctx)?.using_object_streams = true;

    let osd = decode_object_stream_objects(rcx, ctx, &sd, obj_nr)?;

    // Save object stream dict to xRefTableEntry.
    if let Some(e) = ctx.xref.entry_mut(obj_nr) {
        e.object = Some(EntryObject::ObjStm(Box::new(osd)));
    }
    Ok(())
}

/// `materializeObjectStreamType`: a `Type` that refers to a compressed or unloaded object is
/// loaded first.
fn materialize_object_stream_type(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    o: Option<&Object>,
    obj_nr: i64,
) -> Result<(), Error> {
    let Some(Object::Ref(ir)) = o else {
        return Ok(());
    };
    let Some(target) = ctx.xref.find_table_entry_for_ind_ref(*ir) else {
        return Ok(());
    };
    if target.free {
        return Ok(());
    }
    if !target.compressed && (target.object.is_some() || target.offset.is_none()) {
        return Ok(());
    }
    dereferenced_object(rcx, ctx, ir.num)
        .map(|_| ())
        .map_err(|e| e.context(format!("object stream obj#{obj_nr} Type: entry=Type")))
}

/// `validateObjectStreamType`: the object stream has `/Type /ObjStm`.
fn validate_object_stream_type(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    obj_nr: i64,
) -> Result<(), Error> {
    let missing =
        || err_missing_object_stream_entry().context(format!("object stream obj#{obj_nr}"));
    let Some(entry) = ctx.xref.find(obj_nr) else {
        return Err(missing());
    };
    let Some(EntryObject::ObjStm(osd)) = &entry.object else {
        return Err(err_missing_object_stream().context(format!("object stream obj#{obj_nr}")));
    };
    let direct = osd.stream.dict.0.get(b"Type".as_slice()).cloned();
    let found = osd.stream.dict.find(b"Type").cloned();

    materialize_object_stream_type(rcx, ctx, direct.as_ref(), obj_nr)?;

    let (t, found) = ctx
        .xref
        .name_from_value(b"Type", found.as_ref())
        .map_err(|e| e.context(format!("object stream obj#{obj_nr} Type")))?;
    if found && t.as_deref() == Some(b"ObjStm".as_slice()) {
        return Ok(());
    }
    Err(err_corrupt_object_stream().context(format!("object stream obj#{obj_nr}")))
}

/// `activeObjectStreams`: the object streams that current compressed entries refer to, in order.
/// References from superseded revisions are stale.
fn active_object_streams(x: &XRefTable) -> Vec<i64> {
    let streams: BTreeSet<i64> = x
        .table
        .values()
        .filter(|e| !e.free)
        .filter_map(|e| e.object_stream)
        .collect();
    streams.into_iter().collect()
}

/// `decodeObjectStreams`: decodes the prolog of every object stream, so that the objects in them
/// are ready to be used. Entry `Extends` is intentionally left out.
///
/// # Errors
/// The errors of [`decode_object_stream`], and a `Type` that is not `ObjStm`.
pub fn decode_object_streams(rcx: &mut ReadCx<'_>, ctx: &mut Context) -> Result<(), Error> {
    let keys = active_object_streams(&ctx.xref);

    for &obj_nr in &keys {
        rcx.check()?;
        decode_object_stream(rcx, ctx, obj_nr)?;
    }
    for &obj_nr in &keys {
        validate_object_stream_type(rcx, ctx, obj_nr)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Limits};
    use crate::model::Entry;
    use std::sync::atomic::AtomicBool;

    fn ctx_for(input: &[u8], limits: Limits) -> Context {
        let conf = Config {
            limits,
            ..Config::default()
        };
        match Context::new(input.len() as u64, conf) {
            Ok(c) => c,
            Err(e) => panic!("context: {e}"),
        }
    }

    fn content_stream(content: &[u8]) -> StreamDict {
        let mem = crate::ledger::Mem::with_max(1 << 20);
        StreamDict {
            content: Some(mem.bytes_from(content, content.len()).unwrap()),
            ..StreamDict::default()
        }
    }

    // Port of TestDecompressXRefTableEntryClassifiesObjectStreamErrors.
    #[test]
    fn decompress_classifies_object_stream_errors() {
        let compressed = Entry {
            compressed: true,
            object_stream: Some(9),
            object_stream_ind: Some(0),
            ..Entry::default()
        };
        for (name, with_dict, want) in [
            (
                "missing xref entry",
                false,
                err_missing_object_stream_entry(),
            ),
            ("missing object stream", true, err_missing_object_stream()),
        ] {
            let mut x = XRefTable::new(Config::default());
            x.table.insert(7, compressed.clone());
            if with_dict {
                x.table
                    .insert(9, Entry::gen0(Object::Dict(Default::default())));
            }
            let err = decompress_xref_table_entry(&mut x, 7).unwrap_err();
            assert_eq!(err.root(), &want, "{name}");
            assert!(err.to_string().contains("object stream 9"), "{name}: {err}");
        }
    }

    // Port of TestParseObjectStreamClassifiesCorruptDict.
    #[test]
    fn parse_object_stream_classifies_corrupt_dict() {
        let cancel = AtomicBool::new(false);
        let ctx = ctx_for(b"", Limits::default());
        let rcx = ReadCx::new(b"", &cancel, i64::MAX);
        let mut osd = ObjectStreamDict {
            stream: content_stream(b"1"),
            first_obj_offset: 1,
            ..ObjectStreamDict::default()
        };
        let err = parse_object_stream(&rcx, &ctx, &mut osd).unwrap_err();
        assert_eq!(err.root(), &err_corrupt_object_stream_dict());
    }

    // Port of TestParseObjectStreamClassifiesObjectCountLimit.
    #[test]
    fn parse_object_stream_classifies_object_count_limit() {
        let cancel = AtomicBool::new(false);
        let limits = Limits {
            max_object_stream_count: 1,
            ..Limits::default()
        };
        let ctx = ctx_for(b"", limits);
        let rcx = ReadCx::new(b"", &cancel, i64::MAX);
        let content = b"1 0 2 1";
        let mut osd = ObjectStreamDict {
            stream: content_stream(content),
            first_obj_offset: content.len() as i64,
            ..ObjectStreamDict::default()
        };
        let err = parse_object_stream(&rcx, &ctx, &mut osd).unwrap_err();
        assert_eq!(err.root(), &err_object_stream_object_count_limit());
    }

    // Port of TestParseObjectStreamRejectsFirstBeyondDecodeLimit.
    #[test]
    fn parse_object_stream_rejects_first_beyond_decode_limit() {
        let cancel = AtomicBool::new(false);
        let limits = Limits {
            max_decode_bytes: 3,
            ..Limits::default()
        };
        let ctx = ctx_for(b"", limits);
        let rcx = ReadCx::new(b"", &cancel, i64::MAX);
        let raw = b"312030203432>".to_vec(); // hex of "1 0 42"
        let mut osd = ObjectStreamDict {
            stream: StreamDict {
                raw: Some(ctx.xref.mem().bytes_from(&raw, raw.len()).unwrap()),
                filter_pipeline: vec![crate::object::PdfFilter {
                    name: crate::filter::ASCII_HEX.to_vec(),
                    decode_parms: None,
                }],
                ..StreamDict::default()
            },
            first_obj_offset: 4,
            obj_count: 1,
            ..ObjectStreamDict::default()
        };
        let err = parse_object_stream(&rcx, &ctx, &mut osd).unwrap_err();
        assert!(err.is_decode_limit_exceeded(), "{err}");
    }

    #[test]
    fn parse_object_stream_offsets() {
        let cancel = AtomicBool::new(false);
        let ctx = ctx_for(b"", Limits::default());
        let rcx = ReadCx::new(b"", &cancel, i64::MAX);

        // A comment, a NUL separator and a trailing space in the prolog; First is 14.
        let content = b"10 0\x0011 8 %c\n <</A 1>>(hi)";
        let mut osd = ObjectStreamDict {
            stream: content_stream(content),
            first_obj_offset: 14,
            obj_count: 2,
            ..ObjectStreamDict::default()
        };
        parse_object_stream(&rcx, &ctx, &mut osd).unwrap();
        let offs: Vec<i64> = osd
            .obj_array
            .iter()
            .map(|o| if let Object::Integer(i) = o { *i } else { -1 })
            .collect();
        assert_eq!(offs, vec![14, 22]);
        assert_eq!(
            lazy_ref(&osd, 4, 0).unwrap(),
            LazyRef {
                stm: 4,
                start: 14,
                end: 22
            }
        );
        assert_eq!(
            lazy_ref(&osd, 4, 1).unwrap(),
            LazyRef {
                stm: 4,
                start: 22,
                end: -1
            }
        );
        assert!(lazy_ref(&osd, 4, 2).is_err());

        // Offsets that go backwards, or past the content, or a wrong count.
        for (prolog, n) in [
            ("1 8 2 3 ", 2),
            ("1 0 2 99", 2),
            ("1 0 2 x ", 2),
            ("1 0 2 0 ", 3),
        ] {
            let mut c = prolog.as_bytes().to_vec();
            c.extend_from_slice(b"<</A 1>>(hi)");
            let mut osd = ObjectStreamDict {
                stream: content_stream(&c),
                first_obj_offset: prolog.len() as i64,
                obj_count: n,
                ..ObjectStreamDict::default()
            };
            assert!(
                parse_object_stream(&rcx, &ctx, &mut osd).is_err(),
                "{prolog}"
            );
        }
    }

    // Port of TestDecodeObjectStreamRejectsInvalidEntries.
    #[test]
    fn decode_object_stream_rejects_invalid_entries() {
        let with_offset = |e: Entry| Entry {
            offset: Some(0),
            generation: Some(0),
            ..e
        };
        let cases: Vec<(&str, Option<Entry>)> = vec![
            ("missing", None),
            ("empty", Some(Entry::default())),
            (
                "compressed",
                Some(Entry {
                    compressed: true,
                    ..Entry::default()
                }),
            ),
            (
                "compressed with offset",
                Some(with_offset(Entry {
                    compressed: true,
                    ..Entry::default()
                })),
            ),
            (
                "free",
                Some(with_offset(Entry {
                    free: true,
                    ..Entry::default()
                })),
            ),
            (
                "missing offset",
                Some(Entry {
                    generation: Some(0),
                    ..Entry::default()
                }),
            ),
            (
                "missing generation",
                Some(Entry {
                    offset: Some(0),
                    ..Entry::default()
                }),
            ),
        ];
        for (name, entry) in cases {
            let cancel = AtomicBool::new(false);
            let mut ctx = ctx_for(b"", Limits::default());
            let mut rcx = ReadCx::new(b"", &cancel, i64::MAX);
            if let Some(e) = entry {
                ctx.xref.table.insert(7, e);
            }
            let err = decode_object_stream(&mut rcx, &mut ctx, 7).unwrap_err();
            assert_eq!(err.root(), &err_missing_object_stream_entry(), "{name}");
            assert!(
                err.to_string().contains("object stream obj#7"),
                "{name}: {err}"
            );
        }
    }

    // Port of TestDecodeObjectStreamClassifiesCorruptObjectStream.
    #[test]
    fn decode_object_stream_classifies_corrupt_object_stream() {
        let data = b"7 0 obj\n1\nendobj\n";
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(data, Limits::default());
        let mut rcx = ReadCx::new(data, &cancel, i64::MAX);
        ctx.xref.table.insert(
            7,
            Entry {
                offset: Some(0),
                generation: Some(0),
                ..Entry::default()
            },
        );
        let err = decode_object_stream(&mut rcx, &mut ctx, 7).unwrap_err();
        assert_eq!(err.root(), &err_corrupt_object_stream());
        assert!(err.to_string().contains("object stream obj#7"), "{err}");
    }

    fn objstm_file() -> Vec<u8> {
        // The content is the prolog "10 0 11 8 " (First 10), "<</A 1>>" and "(hi)".
        let content = b"10 0 11 8 <</A 1>>(hi)";
        let mut f = b"7 0 obj\n<< /Type /ObjStm /N 2 /First 10 /Length 22 >>\nstream\n".to_vec();
        f.extend_from_slice(content);
        f.extend_from_slice(b"\nendstream\nendobj\n");
        f
    }

    fn compressed(stm: i64, ind: i64) -> Entry {
        Entry {
            compressed: true,
            object_stream: Some(stm),
            object_stream_ind: Some(ind),
            ..Entry::default()
        }
    }

    #[test]
    fn decode_object_streams_and_lazy_objects() {
        let data = objstm_file();
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(&data, Limits::default());
        let mut rcx = ReadCx::new(&data, &cancel, i64::MAX);
        ctx.xref.table.insert(
            7,
            Entry {
                offset: Some(0),
                generation: Some(0),
                ..Entry::default()
            },
        );
        ctx.xref.table.insert(10, compressed(7, 0));
        ctx.xref.table.insert(11, compressed(7, 1));

        decode_object_streams(&mut rcx, &mut ctx).unwrap();
        assert!(matches!(
            ctx.xref.find(7).and_then(|e| e.object.as_ref()),
            Some(EntryObject::ObjStm(_))
        ));
        assert!(ctx.read.as_ref().is_some_and(|r| r.using_object_streams));

        decompress_xref_table_entry(&mut ctx.xref, 10).unwrap();
        decompress_xref_table_entry(&mut ctx.xref, 11).unwrap();
        let e10 = ctx.xref.find(10).unwrap();
        assert!(!e10.compressed && e10.generation == Some(0));
        assert!(matches!(
            e10.object,
            Some(EntryObject::Lazy(LazyRef {
                stm: 7,
                start: 10,
                end: 18
            }))
        ));

        ctx.xref.decode_lazy_entry(10, &cancel).unwrap();
        ctx.xref.decode_lazy_entry(11, &cancel).unwrap();
        match ctx.xref.find(10).and_then(|e| e.object.as_ref()) {
            Some(EntryObject::Obj(Object::Dict(d))) => {
                assert!(matches!(d.find(b"A"), Some(Object::Integer(1))));
            }
            other => panic!("entry 10: {other:?}"),
        }
        assert!(matches!(
            ctx.xref.find(11).and_then(|e| e.object.as_ref()),
            Some(EntryObject::Obj(Object::String(s))) if s == b"hi"
        ));
    }

    #[test]
    fn object_stream_type_must_be_objstm() {
        let mut data = objstm_file();
        let pos = data.windows(5).position(|w| w == b"ObjSt").unwrap();
        data[pos..pos + 6].copy_from_slice(b"ObjXtm");
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(&data, Limits::default());
        let mut rcx = ReadCx::new(&data, &cancel, i64::MAX);
        ctx.xref.table.insert(
            7,
            Entry {
                offset: Some(0),
                generation: Some(0),
                ..Entry::default()
            },
        );
        ctx.xref.table.insert(10, compressed(7, 0));
        let err = decode_object_streams(&mut rcx, &mut ctx).unwrap_err();
        assert_eq!(err.root(), &err_corrupt_object_stream());
    }
}
