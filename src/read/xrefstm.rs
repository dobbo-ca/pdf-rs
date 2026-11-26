//! Cross-reference streams and hybrid files (Go: pkg/pdfcpu/read.go:747-1105).
//!
//! `create_xref_table_entry_from_xref_stream` and `xref_stream_entry_len` (read.go:747-828) are
//! ported here with the functions that call them; the design's line ranges cut between them.

use super::xref::parse_trailer;
use super::{
    PosReader, ReadCx, buffer, err_corrupt_xref_stream, ix, object_buffer_limit,
    parse_object_with_validation_policy, stream,
};
use crate::error::Error;
use crate::model::{Context, Entry, EntryObject};
use crate::object::{Dict, Object, StreamDict, XRefStreamDict};
use crate::parse::{parse_object_attributes, parse_xref_stream_dict_charged};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

/// `createXRefTableEntryFromXRefStream`: the table entry for one row of an xref stream. A
/// compressed entry registers its object stream in `obj_streams`.
pub fn create_xref_table_entry_from_xref_stream(
    entry_type: i64,
    c2: i64,
    c3: i64,
    off_extra: i64,
    obj_streams: &mut BTreeSet<i64>,
    incr: i64,
) -> Entry {
    match entry_type {
        0x00 => {
            // free object
            Entry {
                free: true,
                compressed: false,
                offset: Some(c2),
                generation: Some(c3),
                incr,
                ..Entry::default()
            }
        }

        0x01 => {
            // in use object
            Entry {
                free: false,
                compressed: false,
                offset: Some(c2.wrapping_add(off_extra)),
                generation: Some(c3),
                incr,
                ..Entry::default()
            }
        }

        0x02 => {
            // compressed object; the generation is always 0
            obj_streams.insert(c2);
            Entry {
                free: false,
                compressed: true,
                object_stream: Some(c2),
                object_stream_ind: Some(c3),
                incr,
                ..Entry::default()
            }
        }

        // See 7.5.8.3: unknown entry types resolve to null, represented here by a free entry.
        _ => Entry {
            free: true,
            offset: Some(0),
            generation: Some(0),
            incr,
            ..Entry::default()
        },
    }
}

/// `xRefStreamEntryLen`: the length of one row, the sum of the field widths.
///
/// # Errors
/// An overflowing sum or no field at all.
pub fn xref_stream_entry_len(w: [i64; 3]) -> Result<usize, Error> {
    let invalid = || Error::Corrupt("invalid xref stream W array".to_string());
    let n = w[0].checked_add(w[1]).ok_or_else(invalid)?;
    let n = n.checked_add(w[2]).ok_or_else(invalid)?;
    if n <= 0 {
        return Err(invalid());
    }
    usize::try_from(n).map_err(|_| invalid())
}

/// `bufToInt64`: the big-endian value of `buf` (the high bytes of a wide field are lost).
fn buf_to_i64(buf: &[u8]) -> i64 {
    let mut i: i64 = 0;
    for &b in buf {
        i = (i << 8) | i64::from(b);
    }
    i
}

/// `extractXRefTableEntriesFromXRefStream`: for each object embedded in this xref stream
/// creates the corresponding xref table entry, unless an entry exists already.
///
/// A width of zero means the field is not present and its default is used, if there is one; if
/// the first width is zero, the type field defaults to type 1.
///
/// # Errors
/// An invalid `W` array, and a stream whose length is not a whole number of rows or that has
/// fewer rows than the index has objects; [`Error::Alloc`] for an entry; [`Error::Cancelled`].
pub fn extract_xref_table_entries_from_xref_stream(
    cancel: &AtomicBool,
    buf: &[u8],
    off_extra: i64,
    xsd: &XRefStreamDict,
    ctx: &mut Context,
    incr: i64,
) -> Result<(), Error> {
    let [i1, i2, i3] = xsd.w;

    let entry_len = xref_stream_entry_len(xsd.w)?;

    if !buf.len().is_multiple_of(entry_len) {
        return Err(err_corrupt_xref_stream());
    }

    let obj_count = xsd.objects.len();
    if obj_count > buf.len() / entry_len {
        // Sometimes there is an additional xref entry not accounted for by "Index". We ignore
        // such entries and do not treat this as an error.
        return Err(err_corrupt_xref_stream());
    }

    // Each width is at most the row length, which fits.
    let (i1, i2, i3) = (ix(i1), ix(i2), ix(i3));
    let Context { xref, read, .. } = ctx;
    let read = read.as_mut().ok_or(Error::Panic("missing read context"))?;

    let mut j = 0;
    let mut i = 0;
    while i < buf.len() && j < obj_count {
        if j.is_multiple_of(4096) && cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let obj_nr = xsd.objects[j];

        if !xref.exists(obj_nr) {
            let c1 = if i1 == 0 {
                // If the first element is zero, the type field shall not be present, and shall
                // default to type 1.
                1
            } else {
                buf_to_i64(&buf[i..i + i1])
            };
            let c2 = buf_to_i64(&buf[i + i1..i + i1 + i2]);
            let c3 = buf_to_i64(&buf[i + i1 + i2..i + i1 + i2 + i3]);

            let entry = create_xref_table_entry_from_xref_stream(
                c1,
                c2,
                c3,
                off_extra,
                &mut read.object_streams,
                incr,
            );
            xref.insert_charged(obj_nr, entry)?;
        }
        j += 1;
        i += entry_len;
    }

    Ok(())
}

/// `validateXRefStreamDirectEntries`: the entries that must be direct.
fn validate_xref_stream_direct_entries(d: &Dict, obj_nr: i64) -> Result<(), Error> {
    let direct = |key: &str| -> Result<(), Error> {
        if matches!(d.0.get(key.as_bytes()), Some(Object::Ref(_))) {
            return Err(Error::Corrupt(format!(
                "xref stream obj#{obj_nr} entry {key}: value must be direct"
            )));
        }
        Ok(())
    };
    for key in ["Type", "Size", "Index", "Prev", "W"] {
        direct(key)?;
    }
    if !d.0.contains_key(b"Filter".as_slice()) {
        return Ok(());
    }
    for key in ["Filter", "DecodeParms"] {
        direct(key)?;
    }
    Ok(())
}

/// `xRefStreamDict`: the xref stream dict of the object `o`, with its content loaded and
/// decoded (cross-reference streams are never encrypted).
///
/// # Errors
/// An object that is not a dict, indirect entries that must be direct, a missing length, the
/// filter pipeline, load and decode errors, and the errors of the xref stream dict parser.
pub fn xref_stream_dict(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    o: Object,
    obj_nr: i64,
    stream_offset: i64,
) -> Result<XRefStreamDict, Error> {
    let Object::Dict(mut d) = o else {
        return Err(Error::Corrupt("missing xref stream dictionary".to_string())
            .context(format!("xref stream obj#{obj_nr}")));
    };
    validate_xref_stream_direct_entries(&d, obj_nr)?;

    // Parse attributes for stream object.
    let (stream_length, stream_length_obj_nr) = d.length();
    if stream_length.is_none() && stream_length_obj_nr.is_none() {
        return Err(Error::Corrupt("missing xref stream Length".to_string())
            .context(format!("xref stream obj#{obj_nr}")));
    }

    let filter_pipeline = stream::pdf_filter_pipeline(rcx, ctx, &mut d)?;

    // We have a stream object.
    let mut sd = StreamDict::new(
        d,
        stream_offset,
        stream_length,
        stream_length_obj_nr,
        filter_pipeline,
    );

    stream::load_encoded_stream_content(rcx, ctx, &mut sd, false)?;

    // Cross-reference streams are never encrypted; decode with the caller's resource limit.
    ctx.xref.decode_stream(&mut sd, rcx.cancel).map_err(|e| {
        e.context(format!(
            "xRefStreamDict: cannot decode stream for obj#:{obj_nr}"
        ))
    })?;

    let declared_size = sd.size();
    // Relaxed: an /Index that goes past /Size raises the size.
    let mut xsd = parse_xref_stream_dict_charged(&sd, &ctx.xref.conf.limits, &mut ctx.xref.held)?;
    if let Some(declared) = declared_size
        && declared != xsd.size
    {
        let size = xsd.size;
        xsd.stream.dict.update(b"Size", Object::Integer(size));
    }
    Ok(xsd)
}

/// `processXRefStream`: reads the trailer and the entries of an xref stream, and returns the
/// offset of the previous section. `offset` moves by `off_extra`. The stream dict becomes the
/// object of its own table entry if that entry points at it.
///
/// # Errors
/// The trailer errors and the errors of [`extract_xref_table_entries_from_xref_stream`].
pub fn process_xref_stream(
    cancel: &AtomicBool,
    ctx: &mut Context,
    xsd: XRefStreamDict,
    obj_nr: i64,
    offset: &mut i64,
    off_extra: i64,
    incr: i64,
) -> Result<Option<i64>, Error> {
    let had_id = ctx.xref.id.is_some();
    parse_trailer(&mut ctx.xref, &xsd.stream.dict)?;
    // Go keeps the trailer's two-element `ID` array itself, shared with this dictionary.
    let shares_id = !had_id
        && xsd
            .stream
            .dict
            .array_entry(b"ID")
            .is_some_and(|a| a.len() == 2);

    // Parse xRefStream and create xRefTable entries for embedded objects.
    let content = xsd.stream.content.clone();
    let buf: &[u8] = content.as_ref().map_or(&[][..], |b| b.as_slice());
    extract_xref_table_entries_from_xref_stream(cancel, buf, off_extra, &xsd, ctx, incr)?;

    *offset = offset.wrapping_add(off_extra);

    let prev_offset = xsd.previous_offset;

    if let Some(entry) = ctx.xref.table.get_mut(&obj_nr)
        && entry.offset == Some(*offset)
    {
        entry.object = Some(EntryObject::XRefStm(Box::new(xsd)));
        if shares_id {
            ctx.xref.id_xref_stm = Some(obj_nr);
        }
    }

    Ok(prev_offset)
}

/// `parseXRefStream`: parses the xref stream at `rd` and sets up xref table entries for all
/// embedded objects and the xref stream dict. Returns the offset of the previous section.
/// `offset` is where `rd` was positioned.
///
/// # Errors
/// The object buffer errors, a missing stream marker, the parse errors, and the errors of
/// [`xref_stream_dict`] and [`process_xref_stream`].
pub fn parse_xref_stream(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    rd: &mut PosReader<'_>,
    offset: &mut i64,
    off_extra: i64,
    incr: i64,
) -> Result<Option<i64>, Error> {
    let mem = ctx.xref.mem().clone();
    let b = buffer(rcx, &mem, rd, object_buffer_limit(&ctx.xref.conf.limits))?;
    let (end_ind, stream_ind) = (b.end_ind, b.stream_ind);

    // We expect a stream and therefore "stream" before "endobj" if "endobj" within buffer.
    // There is no guarantee that "endobj" is contained in this buffer for large streams!
    if stream_ind < 0 || (end_ind > 0 && end_ind < stream_ind) {
        return Err(err_corrupt_xref_stream().context("xref stream object"));
    }

    // Init object parse buf.
    let mut l: &[u8] = &b.buf.v[..ix(stream_ind)];

    let (obj_nr, _gen_nr) = parse_object_attributes(&mut l)?;

    // parse this object
    let o = parse_object_with_validation_policy(rcx, ctx, &mut l)
        .map_err(|e| e.context("parseXRefStream: no object"))?;

    let stream_offset = b.stream_offset.wrapping_add(*offset);
    let xsd = xref_stream_dict(rcx, ctx, o, obj_nr, stream_offset)?;

    process_xref_stream(rcx.cancel, ctx, xsd, obj_nr, offset, off_extra, incr)
}

/// `parseHybridXRefStream`: parses the xref stream of a hybrid file at `offset`.
///
/// # Errors
/// A negative offset and the errors of [`parse_xref_stream`].
pub fn parse_hybrid_xref_stream(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    offset: &mut i64,
    off_extra: i64,
    incr: i64,
) -> Result<(), Error> {
    let mut rd = PosReader::at(rcx.data, *offset)?;
    parse_xref_stream(rcx, ctx, &mut rd, offset, off_extra, incr)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Limits};
    use crate::error::Sentinel;
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

    fn xsd(objects: Vec<i64>, w: [i64; 3]) -> XRefStreamDict {
        XRefStreamDict {
            objects,
            w,
            ..XRefStreamDict::default()
        }
    }

    // Port of TestExtractXRefStreamEntriesDefaultType.
    #[test]
    fn extract_default_type() {
        let mut ctx = ctx_for(b"", Limits::default());
        const OBJ_COUNT: i64 = 67;
        let objects: Vec<i64> = (0..OBJ_COUNT).collect();
        let mut buf = Vec::new();
        for obj_nr in 0..OBJ_COUNT {
            let offset = 1 + obj_nr * 257;
            buf.extend_from_slice(&[(offset >> 16) as u8, (offset >> 8) as u8, offset as u8]);
        }
        extract_xref_table_entries_from_xref_stream(
            &AtomicBool::new(false),
            &buf,
            0,
            &xsd(objects, [0, 3, 0]),
            &mut ctx,
            0,
        )
        .unwrap();
        assert_eq!(ctx.xref.table.len(), OBJ_COUNT as usize);
        for obj_nr in 0..OBJ_COUNT {
            let entry = &ctx.xref.table[&obj_nr];
            assert!(!entry.free && !entry.compressed, "object {obj_nr}");
            assert_eq!(entry.offset, Some(1 + obj_nr * 257), "object {obj_nr}");
        }
    }

    #[test]
    fn extract_stops_on_cancel() {
        let mut ctx = ctx_for(b"", Limits::default());
        let err = extract_xref_table_entries_from_xref_stream(
            &AtomicBool::new(true),
            &[0, 0, 9],
            0,
            &xsd(vec![1], [0, 3, 0]),
            &mut ctx,
            0,
        )
        .unwrap_err();
        assert_eq!(err, Error::Cancelled);
        assert!(!ctx.xref.exists(1));
    }

    // Port of TestExtractXRefStreamEntriesRejectsZeroWidths.
    #[test]
    fn extract_rejects_zero_widths() {
        let mut ctx = ctx_for(b"", Limits::default());
        let err = extract_xref_table_entries_from_xref_stream(
            &AtomicBool::new(false),
            &[],
            0,
            &xsd(vec![], [0, 0, 0]),
            &mut ctx,
            0,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("invalid xref stream W array"),
            "{err}"
        );
        assert!(xref_stream_entry_len([i64::MAX, 1, 0]).is_err());
        assert_eq!(xref_stream_entry_len([1, 2, 3]), Ok(6));
    }

    // Port of TestExtractXRefStreamEntriesClassifiesCorruptStream.
    #[test]
    fn extract_classifies_a_corrupt_stream() {
        let mut ctx = ctx_for(b"", Limits::default());
        let corrupt = err_corrupt_xref_stream();
        let err = extract_xref_table_entries_from_xref_stream(
            &AtomicBool::new(false),
            &[1, 2],
            0,
            &xsd(vec![0], [1, 1, 1]),
            &mut ctx,
            0,
        )
        .unwrap_err();
        assert_eq!(err, corrupt);
        // Fewer rows than objects.
        let err = extract_xref_table_entries_from_xref_stream(
            &AtomicBool::new(false),
            &[1, 2, 3],
            0,
            &xsd(vec![0, 1], [1, 1, 1]),
            &mut ctx,
            0,
        )
        .unwrap_err();
        assert_eq!(err, corrupt);
    }

    #[test]
    fn rows_make_free_in_use_and_compressed_entries() {
        let mut ctx = ctx_for(b"", Limits::default());
        // W [1 2 1]: free (next 0, gen 255), in use at 300 (gen 1), compressed in stream 9 at
        // index 4.
        let buf = [0, 0, 0, 255, 1, 1, 44, 1, 2, 0, 9, 4];
        extract_xref_table_entries_from_xref_stream(
            &AtomicBool::new(false),
            &buf,
            10,
            &xsd(vec![0, 5, 6], [1, 2, 1]),
            &mut ctx,
            3,
        )
        .unwrap();
        let t = &ctx.xref.table;
        assert!(t[&0].free);
        assert_eq!((t[&0].offset, t[&0].generation), (Some(0), Some(255)));
        assert_eq!(
            (t[&5].offset, t[&5].generation, t[&5].incr),
            (Some(310), Some(1), 3)
        );
        assert!(t[&6].compressed);
        assert_eq!(
            (t[&6].object_stream, t[&6].object_stream_ind),
            (Some(9), Some(4))
        );
        assert!(t[&6].offset.is_none() && t[&6].generation.is_none());
        assert_eq!(ctx.read.as_ref().map(|r| r.object_streams.len()), Some(1));
        assert!(
            ctx.read
                .as_ref()
                .is_some_and(|r| r.is_object_stream_object(9))
        );
    }

    // Port of TestXRefStreamUnknownEntryTypes.
    #[test]
    fn unknown_entry_types_are_null_free_entries() {
        for entry_type in [3, 255, 256, i64::MAX, -1] {
            let mut streams = BTreeSet::new();
            let entry = create_xref_table_entry_from_xref_stream(
                entry_type,
                123,
                456,
                789,
                &mut streams,
                2,
            );
            assert!(entry.free && !entry.compressed, "type {entry_type}");
            assert_eq!(
                (entry.offset, entry.generation, entry.incr),
                (Some(0), Some(0), 2)
            );
            assert!(
                streams.is_empty(),
                "unknown entry registered an object stream"
            );

            // An older in-use entry must not replace the newer null entry.
            let mut ctx = ctx_for(b"", Limits::default());
            ctx.xref.table.insert(7, entry);
            extract_xref_table_entries_from_xref_stream(
                &AtomicBool::new(false),
                &[1, 42, 0],
                0,
                &xsd(vec![7], [1, 1, 1]),
                &mut ctx,
                3,
            )
            .unwrap();
            assert!(ctx.xref.table[&7].free);
            assert_eq!(ctx.xref.table[&7].incr, 2);
            ctx.xref.ensure_valid_free_list().unwrap();
        }
    }

    #[test]
    fn a_wide_field_loses_its_high_bytes() {
        assert_eq!(buf_to_i64(&[0, 0, 0, 0, 0, 0, 0, 0, 1, 2]), 0x0102);
        assert_eq!(buf_to_i64(&[]), 0);
        assert_eq!(buf_to_i64(&[0xFF; 8]), -1);
    }

    #[test]
    fn indirect_entries_of_an_xref_stream_must_be_direct() {
        let mut d = Dict::default();
        d.set(
            b"Size",
            Object::Ref(crate::object::Ref {
                num: 3,
                generation: 0,
            }),
        );
        let err = validate_xref_stream_direct_entries(&d, 4).unwrap_err();
        assert!(
            err.to_string().contains("xref stream obj#4 entry Size"),
            "{err}"
        );
        let mut d = Dict::default();
        d.set(
            b"Filter",
            Object::Ref(crate::object::Ref {
                num: 3,
                generation: 0,
            }),
        );
        assert!(validate_xref_stream_direct_entries(&d, 4).is_err());
        // Without a Filter, DecodeParms is not looked at.
        let mut d = Dict::default();
        d.set(
            b"DecodeParms",
            Object::Ref(crate::object::Ref {
                num: 3,
                generation: 0,
            }),
        );
        assert!(validate_xref_stream_direct_entries(&d, 4).is_ok());
    }

    // Port of TestXRefStreamDictErrorsIncludeObjectContext.
    #[test]
    fn xref_stream_dict_errors_include_the_object() {
        let input = b"";
        let mut ctx = ctx_for(input, Limits::default());
        let cancel = AtomicBool::new(false);
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);

        let err = xref_stream_dict(&mut rcx, &mut ctx, Object::Integer(1), 7, 0).unwrap_err();
        assert!(
            err.to_string().contains("missing xref stream dictionary"),
            "{err}"
        );
        assert!(err.to_string().contains("xref stream obj#7"), "{err}");

        let err =
            xref_stream_dict(&mut rcx, &mut ctx, Object::Dict(Dict::default()), 8, 0).unwrap_err();
        assert!(
            err.to_string().contains("missing xref stream Length"),
            "{err}"
        );
        assert!(err.to_string().contains("xref stream obj#8"), "{err}");
    }

    // Port of TestParseXRefStreamClassifiesCorruptStreamObject.
    #[test]
    fn parse_xref_stream_classifies_a_corrupt_stream_object() {
        let input = b"1 0 obj\n<<>>\nendobj\n";
        let mut ctx = ctx_for(input, Limits::default());
        let cancel = AtomicBool::new(false);
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        let mut rd = PosReader::at(input, 0).unwrap();
        let mut offset = 0;
        let err = parse_xref_stream(&mut rcx, &mut ctx, &mut rd, &mut offset, 0, 0).unwrap_err();
        assert!(err.to_string().contains("corrupt xref stream"), "{err}");
        assert!(err.to_string().contains("xref stream object"), "{err}");
    }

    // Port of TestXRefStreamHonorsBufferLimit.
    #[test]
    fn parse_xref_stream_honors_the_buffer_limit() {
        let input = b"1 0 obj << /Type /XRef /Length 0 >> stream\n";
        let mut ctx = ctx_for(
            input,
            Limits {
                max_object_bytes: 16,
                ..Limits::default()
            },
        );
        let cancel = AtomicBool::new(false);
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        let mut rd = PosReader::at(input, 0).unwrap();
        let mut offset = 0;
        let err = parse_xref_stream(&mut rcx, &mut ctx, &mut rd, &mut offset, 0, 0).unwrap_err();
        assert!(err.is(Sentinel::ObjectBufferLimit), "{err}");
    }

    #[test]
    fn process_xref_stream_registers_itself_when_its_entry_points_at_it() {
        let mut ctx = ctx_for(b"", Limits::default());
        let mut dict = Dict::default();
        dict.set(b"Size", Object::Integer(2));
        dict.set(
            b"Root",
            Object::Ref(crate::object::Ref {
                num: 1,
                generation: 0,
            }),
        );
        let mut x = xsd(vec![1], [1, 1, 1]);
        x.stream.dict = dict;
        x.previous_offset = Some(77);
        x.stream.content = Some(
            ctx.xref
                .mem()
                .bytes_from(&[1, 9, 0], 3)
                .unwrap_or_else(|e| panic!("{e}")),
        );
        // The entry of object 4 is at the stream's offset once `off_extra` is added.
        ctx.xref.table.insert(
            4,
            Entry {
                offset: Some(15),
                ..Entry::default()
            },
        );
        let mut offset = 10;
        let prev = process_xref_stream(&AtomicBool::new(false), &mut ctx, x, 4, &mut offset, 5, 1)
            .unwrap();
        assert_eq!((prev, offset), (Some(77), 15));
        assert_eq!(ctx.xref.table[&1].offset, Some(14));
        assert!(matches!(
            ctx.xref.table[&4].object,
            Some(EntryObject::XRefStm(_))
        ));
        assert_eq!(ctx.xref.size, Some(2));
    }

    // Go's trailer ID is the array of the newest xref stream's dictionary (read.go:1220).
    #[test]
    fn process_xref_stream_records_the_stream_that_holds_the_id() {
        let mut ctx = ctx_for(b"", Limits::default());
        for (num, offset) in [(4, 15), (6, 30)] {
            ctx.xref.table.insert(
                num,
                Entry {
                    offset: Some(offset),
                    ..Entry::default()
                },
            );
        }
        let id = Object::Array(vec![
            Object::Hex(b"AA".to_vec()),
            Object::Hex(b"BB".to_vec()),
        ]);
        let mut x = xsd(Vec::new(), [1, 1, 1]);
        x.stream.dict.set(b"Size", Object::Integer(7));
        x.stream.dict.set(
            b"Root",
            Object::Ref(crate::object::Ref {
                num: 1,
                generation: 0,
            }),
        );
        x.stream.dict.set(b"ID", id);
        let older = x.clone();

        let mut offset = 15;
        process_xref_stream(&AtomicBool::new(false), &mut ctx, x, 4, &mut offset, 0, 0).unwrap();
        assert_eq!(ctx.xref.id_xref_stm, Some(4));
        // An older section with an ID of its own does not share the one already taken.
        let mut offset = 30;
        process_xref_stream(
            &AtomicBool::new(false),
            &mut ctx,
            older,
            6,
            &mut offset,
            0,
            0,
        )
        .unwrap();
        assert_eq!(ctx.xref.id_xref_stm, Some(4));
    }
}
