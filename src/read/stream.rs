//! The stream half of the read path (Go: pkg/pdfcpu/read.go:2494-2720 and :3150-3551, plus
//! `loadStreamDict` at :3891; docs/design.md section 15, WP 8): the filter pipeline of a stream
//! dictionary, reading the encoded content, decrypting it, and decoding it.
//!
//! Go's validation mode is always relaxed here, so the filter aliases of inline images are always
//! normalized. `ctx.Read.BinaryTotalSize` is not kept (statistics only; `ReadInfo` has no field
//! for it).

use super::{
    ChargedBuf, DEFAULT_BUF_SIZE, MAX_BUF_SIZE, PosReader, ReadCx, charge_object_read,
    dereferenced_dict, dereferenced_integer, dereferenced_object, find, grow_buf_by, lim, to_i64,
};
use crate::crypto::{decrypt_stream, stream_crypt_method};
use crate::error::{Error, Sentinel};
use crate::filter;
use crate::gocap::roundupsize;
use crate::ledger::{Bytes, Mem};
use crate::model::Context;
use crate::model::deref::type_name;
use crate::object::{
    Array, Dict, Object, PdfFilter, StreamDict, array_copy_bytes, dict_copy_bytes,
};
use std::sync::atomic::AtomicBool;

fn corrupt(msg: String) -> Error {
    Error::Corrupt(msg)
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

// ---------------------------------------------------------------------------------------------
// The filter pipeline

/// `inlineImageFilterAliases`: the canonical filter name of an inline image abbreviation.
fn alias_target(name: &[u8]) -> Option<&'static [u8]> {
    Some(match name {
        b"AHx" => filter::ASCII_HEX,
        b"A85" => filter::ASCII85,
        b"LZW" => filter::LZW,
        b"Fl" => filter::FLATE,
        b"RL" => filter::RUN_LENGTH,
        b"CCF" => filter::CCITT_FAX,
        b"DCT" => filter::DCT,
        _ => return None,
    })
}

/// `normalizeStreamFilterArray`: repaired names are written back to `filters` (the `Filter`
/// array) and to the pipeline.
fn normalize_stream_filter_array(
    ctx: &mut Context,
    filters: &mut Array,
    pipeline: &mut [PdfFilter],
) -> Result<(), Error> {
    for (slot, f) in filters.iter_mut().zip(pipeline) {
        if let Some(target) = alias_target(&f.name) {
            ctx.xref.held.grow_by(2 * target.len() as u64)?;
            *slot = Object::name(target);
            f.name = target.to_vec();
        }
    }
    Ok(())
}

/// `filterNames`: the pipeline of `filter_array` without decode parameters, its names as given.
/// It is charged to the table before it is built.
fn filter_names(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    filter_array: &Array,
) -> Result<Vec<PdfFilter>, Error> {
    let mut pipeline: Vec<PdfFilter> = Vec::new();
    ctx.xref
        .held
        .grow_by((size_of::<PdfFilter>() * filter_array.len()) as u64)?;
    pipeline.try_reserve_exact(filter_array.len())?;
    for (i, o) in filter_array.iter().enumerate() {
        let deref;
        let o = match o {
            Object::Ref(r) => {
                deref = dereferenced_object(rcx, ctx, r.num)
                    .map_err(|e| e.context(format!("filter pipeline entry {i} obj#{}", r.num)))?;
                &deref
            }
            other => other,
        };
        let Object::Name(name) = o else {
            return Err(corrupt(format!(
                "filter pipeline entry {i}: corrupt filter array, got {}",
                type_name(o)
            )));
        };
        ctx.xref.held.grow_by(name.len() as u64)?;
        pipeline.push(PdfFilter {
            name: name.clone(),
            decode_parms: None,
        });
    }
    Ok(pipeline)
}

/// `buildFilterPipeline`: adds the decode parameters of `decode_parms_arr` to `pipeline`, each
/// charged to the table before it is copied.
fn build_filter_pipeline(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    pipeline: &mut [PdfFilter],
    decode_parms_arr: Option<&Array>,
) -> Result<(), Error> {
    let Some(arr) = decode_parms_arr else {
        return Ok(());
    };
    for (i, (f, o)) in pipeline.iter_mut().zip(arr).enumerate() {
        let dict = match o {
            Object::Null => continue,
            Object::Dict(d) => {
                ctx.xref.held.grow_by(dict_copy_bytes(d))?;
                d.clone()
            }
            Object::Ref(r) => {
                let d = dereferenced_dict(rcx, ctx, r.num)
                    .map_err(|e| e.context(format!("filter pipeline DecodeParms entry {i}")))?;
                ctx.xref.held.grow_by(dict_copy_bytes(&d))?;
                d
            }
            _ => {
                return Err(corrupt(format!(
                    "filter pipeline DecodeParms entry {i}: corrupt decode parameters"
                )));
            }
        };
        f.decode_parms = Some(dict);
    }
    Ok(())
}

/// `singleFilter`: the pipeline of a stream with one filter name. An empty `DecodeParms`
/// dictionary is no parameters. It is charged to the table before it is built.
fn single_filter(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    filter_name: &[u8],
    d: &Dict,
) -> Result<Vec<PdfFilter>, Error> {
    let filter_name = alias_target(filter_name).unwrap_or(filter_name);
    let bad = || {
        corrupt(format!(
            "single filter {} DecodeParms: corrupt decode parameters",
            lossy(filter_name)
        ))
    };
    let deref;
    let obj = match d.find(b"DecodeParms") {
        // W/o decode parameters.
        None => return one_filter(ctx, filter_name, None),
        Some(Object::Ref(r)) => {
            deref = dereferenced_object(rcx, ctx, r.num)
                .map_err(|e| e.context("single filter DecodeParms"))?;
            &deref
        }
        Some(o) => o,
    };
    let parms = match obj {
        Object::Dict(d) => Some(d),
        Object::Array(arr) => {
            if arr.len() > 1 {
                return Err(bad());
            }
            match arr.first() {
                None | Some(Object::Null) => None,
                Some(Object::Dict(d)) => Some(d),
                Some(_) => return Err(bad()),
            }
        }
        _ => return Err(bad()),
    };
    one_filter(ctx, filter_name, parms.filter(|d| !d.is_empty()))
}

/// A pipeline of the one filter `name` with `parms`, charged to the table before it is built.
fn one_filter(
    ctx: &mut Context,
    name: &[u8],
    parms: Option<&Dict>,
) -> Result<Vec<PdfFilter>, Error> {
    let n = size_of::<PdfFilter>() as u64 + name.len() as u64 + parms.map_or(0, dict_copy_bytes);
    ctx.xref.held.grow_by(n)?;
    Ok(vec![PdfFilter {
        name: name.to_vec(),
        decode_parms: parms.cloned(),
    }])
}

/// `filterArraySupportsDecodeParms`.
fn filter_array_supports_decode_parms(pipeline: &[PdfFilter]) -> bool {
    pipeline.iter().any(|f| {
        let name = alias_target(&f.name).unwrap_or(f.name.as_slice());
        name == b"Crypt" || filter::supports_decode_parms(name)
    })
}

/// `pdfFilterPipeline` (read.go:2651): the filter pipeline of a stream dict. Normalizing a
/// filter alias changes the `Filter` entry of `dict`, so it is mutable. The pipeline is charged
/// to the table, which keeps it in the stream dict.
///
/// # Errors
/// A corrupt `Filter` or `DecodeParms`, and the errors of dereferencing what they refer to.
pub fn pdf_filter_pipeline(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    dict: &mut Dict,
) -> Result<Vec<PdfFilter>, Error> {
    // A referenced `Filter` is dereferenced into `deref`; a direct one is borrowed.
    let deref = match dict.find(b"Filter") {
        // Stream is not compressed.
        None => return Ok(Vec::new()),
        // Compressed stream.
        Some(Object::Ref(r)) => Some(
            dereferenced_object(rcx, ctx, r.num)
                .map_err(|e| e.context("filter pipeline Filter"))?,
        ),
        Some(_) => None,
    };
    let Some(o) = deref.as_ref().or_else(|| dict.find(b"Filter")) else {
        return Ok(Vec::new());
    };

    if let Object::Name(name) = o {
        // `normalizeStreamFilterName`: a repaired name is written back to the `Filter` entry.
        if let Some(target) = alias_target(name) {
            dict.update(b"Filter", Object::name(target));
            return single_filter(rcx, ctx, target, dict);
        }
        return single_filter(rcx, ctx, name, dict);
    }

    // Filter pipeline: an array of filter names.
    let Object::Array(filter_array) = o else {
        return Err(corrupt("filter pipeline: corrupt filter array".to_string()));
    };
    let mut pipeline = filter_names(rcx, ctx, filter_array)?;

    // Optional array of decode parameter dicts.
    let mut use_parms = false;
    if let Some(Object::Array(a)) = dict.find(b"DecodeParms")
        && filter_array_supports_decode_parms(&pipeline)
    {
        if a.len() != pipeline.len() {
            return Err(corrupt(
                "filter pipeline DecodeParms: corrupt decode parameters".to_string(),
            ));
        }
        use_parms = true;
    }

    if pipeline.iter().any(|f| alias_target(&f.name).is_some()) {
        match deref {
            // The referenced array becomes the dict's own.
            Some(Object::Array(mut a)) => {
                ctx.xref.held.grow_by(array_copy_bytes(&a))?;
                normalize_stream_filter_array(ctx, &mut a, &mut pipeline)?;
                dict.update(b"Filter", Object::Array(a));
            }
            _ => {
                if let Some(Object::Array(a)) = dict.find_mut(b"Filter") {
                    normalize_stream_filter_array(ctx, a, &mut pipeline)?;
                }
            }
        }
    }

    let decode_parms_arr = match dict.find(b"DecodeParms") {
        Some(Object::Array(a)) if use_parms => Some(a),
        _ => None,
    };
    build_filter_pipeline(rcx, ctx, &mut pipeline, decode_parms_arr)?;
    Ok(pipeline)
}

// ---------------------------------------------------------------------------------------------
// Reading the encoded content

/// A charged copy of `src`. `charge` is what Go allocated (at least `src.len()`), `go_cap` the
/// capacity of the Go slice.
fn charged_bytes(mem: &Mem, src: &[u8], charge: usize, go_cap: usize) -> Result<Bytes, Error> {
    let held = mem.charge(charge as u64)?;
    let mut v: Vec<u8> = Vec::new();
    v.try_reserve_exact(src.len())?;
    v.extend_from_slice(src);
    Ok(Bytes::new(v, go_cap, Some(held)))
}

/// `readStreamContentBlindly`: reads up to `endstream` for a stream of unknown length, within
/// `max_stream_bytes`. The result has no trailing end of line.
fn read_stream_content_blindly(
    rd: &mut PosReader<'_>,
    mem: &Mem,
    max_stream_bytes: i64,
) -> Result<Bytes, Error> {
    // Weak heuristic for reading in stream data for cases where stream length is unknown.
    // ...data...{eol}endstream{eol}endobj
    let mut grow_size = DEFAULT_BUF_SIZE;
    if to_i64(grow_size) > max_stream_bytes {
        grow_size = usize::try_from(max_stream_bytes.max(0)).unwrap_or(0);
    }

    let mut buf = ChargedBuf::new(mem)?;
    grow_buf_by(&mut buf, grow_size, rd)?;

    let mut i = find(&buf.v, b"endstream");
    while i.is_none() {
        if to_i64(buf.v.len()) >= max_stream_bytes {
            return Err(corrupt(format!(
                "stream content exceeds maximum blind read length {max_stream_bytes}"
            )));
        }

        grow_size = grow_size.saturating_mul(2).min(MAX_BUF_SIZE);

        let remaining = max_stream_bytes.saturating_sub(to_i64(buf.v.len()));
        if to_i64(grow_size) > remaining {
            grow_size = usize::try_from(remaining).unwrap_or(0);
        }

        grow_buf_by(&mut buf, grow_size, rd)?;
        i = find(&buf.v, b"endstream");
    }

    let mut end = i.unwrap_or(0);

    // Cut off trailing eol's.
    while end > 0 && matches!(buf.v.get(end - 1), Some(0x0A | 0x0D)) {
        end -= 1;
    }

    // Keep only the stream data; the read buffer can be much larger. `bytes.Clone` grows an
    // empty slice by append, so the capacity is the size class.
    let data = buf.v.get(..end).unwrap_or(&[]);
    let go_cap = if data.is_empty() {
        0
    } else {
        roundupsize(data.len())
    };
    charged_bytes(mem, data, data.len(), go_cap)
}

/// `streamDataBeforeEOF`: the data of `buf` up to `endstream`, for an input that ends before the
/// stream length; the end of the input if there is no marker.
fn stream_data_before_eof(buf: &[u8]) -> Result<&[u8], Error> {
    match find(buf, b"endstream") {
        Some(i) => Ok(&buf[..i]),
        None => Err(Error::Sentinel(Sentinel::Eof)),
    }
}

/// `readStreamContent`: the encoded content of `stream_length` bytes at `rd`. `available` is the
/// number of input bytes at the stream offset; the buffer never exceeds it. A length of 0 or
/// less reads up to `endstream`.
///
/// # Errors
/// A length over `max_stream_bytes`, an input that ends before the length without an
/// `endstream` marker ([`Sentinel::Eof`]), a blind read that finds no marker within the limit,
/// the ledger's errors.
pub fn read_stream_content(
    rd: &mut PosReader<'_>,
    mem: &Mem,
    stream_length: i64,
    available: i64,
    max_stream_bytes: i64,
) -> Result<Bytes, Error> {
    if stream_length <= 0 {
        // Read until "endstream" then fix "Length", but do not allow unbounded memory growth for
        // missing or corrupt markers.
        return read_stream_content_blindly(rd, mem, max_stream_bytes);
    }

    if stream_length > max_stream_bytes {
        return Err(corrupt(format!(
            "stream length {stream_length} exceeds maximum supported length {max_stream_bytes}"
        )));
    }

    // A stream length beyond the end of the input is corrupt; allocate only what the input can
    // supply.
    let buf_len_i = stream_length.min(available.max(0));
    let buf_len = usize::try_from(buf_len_i).map_err(|_| Error::OutOfMemory)?;
    let buf = rd.take(buf_len);

    if buf.len() < buf_len || buf_len_i < stream_length {
        // The input ended before the stream length.
        let before = stream_data_before_eof(buf)?;
        return charged_bytes(mem, before, buf_len, buf_len);
    }

    charged_bytes(mem, buf, buf_len, buf_len)
}

/// `ensureStreamLength`: `Length` is the length of the encoded content if it differs, or if
/// `fix_length`.
fn ensure_stream_length(sd: &mut StreamDict, fix_length: bool) {
    let l = sd.raw.as_ref().map_or(0, |r| to_i64(r.len()));
    if fix_length || sd.stream_length.is_none_or(|n| n != l) {
        sd.stream_length = Some(l);
        sd.dict.set(b"Length", Object::Integer(l));
    }
}

/// `ensureIndirectStreamLength`: an indirect `Length` is read before the content.
fn ensure_indirect_stream_length(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    sd: &mut StreamDict,
    fix_length: bool,
) -> Result<(), Error> {
    if fix_length || sd.stream_length.is_some() {
        return Ok(());
    }
    let Some(obj_nr) = sd.stream_length_obj_nr else {
        // Relaxed mode: a missing stream length is skipped.
        return Ok(());
    };

    match dereferenced_integer(rcx, ctx, obj_nr) {
        Ok(n) => sd.stream_length = Some(n),
        Err(e) if e.is(Sentinel::ReferenceDoesNotExist) => sd.stream_length = None,
        Err(e) => return Err(e),
    }
    Ok(())
}

/// `loadEncodedStreamContent` (read.go:3346): reads the encoded content of `sd` from the input
/// into `sd.raw`, fixing a wrong length if `fix_length`.
///
/// # Errors
/// The errors of the `Length` dereference, the object read budget, `MaxStreamTotalBytes`, and
/// [`read_stream_content`].
pub fn load_encoded_stream_content(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    sd: &mut StreamDict,
    fix_length: bool,
) -> Result<(), Error> {
    if sd.raw.is_some() {
        return Ok(());
    }

    // Read stream content encoded at offset with stream length. Dereference stream length if
    // stream length is an indirect object.
    ensure_indirect_stream_length(rcx, ctx, sd, fix_length)
        .map_err(|e| e.context("stream length"))?;

    let mut rd = PosReader::at(rcx.data, sd.stream_offset)
        .map_err(|e| e.context("position stream reader"))?;

    let l1 = if fix_length {
        0
    } else {
        sd.stream_length.unwrap_or(0)
    };

    // Stream data counts toward the read budget: before allocating for a known length, after
    // reading otherwise.
    let file_size = i64::try_from(ctx.read_file_size()).unwrap_or(i64::MAX);
    let available = file_size.saturating_sub(sd.stream_offset).max(0);
    if l1 > 0 {
        let n = l1.min(available);
        charge_object_read(rcx, n)?;
        ctx.xref.charge_stream_bytes(n)?;
    }

    let mem = ctx.xref.mem().clone();
    let max_stream_bytes = lim(ctx.xref.conf.limits.max_stream_bytes);
    let raw = read_stream_content(&mut rd, &mem, l1, available, max_stream_bytes)
        .map_err(|e| e.context("read stream content"))?;
    let raw_len = to_i64(raw.len());
    sd.raw = Some(raw);

    if l1 <= 0 {
        charge_object_read(rcx, raw_len)?;
        ctx.xref.charge_stream_bytes(raw_len)?;
    }

    ensure_stream_length(sd, fix_length);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Decrypting and decoding

/// `metadataStream`: the stream is a `/Type /Metadata` stream.
fn metadata_stream(ctx: &mut Context, sd: &StreamDict) -> Result<bool, Error> {
    let (t, _) = ctx
        .xref
        .dereference_name_entry(&sd.dict, b"Type")
        .map_err(|e| e.context("metadata stream Type"))?;
    Ok(t.as_deref() == Some(b"Metadata".as_slice()))
}

/// `repairablePlaintextMetadata`: an AES stream of plaintext metadata that is too short or not
/// block aligned.
fn repairable_plaintext_metadata(ctx: &Context, metadata: bool, err: &Error) -> bool {
    let Some(e) = &ctx.xref.e else {
        return false;
    };
    if !e.emd || !ctx.xref.aes4_streams || !metadata {
        return false;
    }
    err.is(Sentinel::AesTooShort) || err.is(Sentinel::AesUnaligned)
}

/// `validPlaintextMetadata`: the stream decodes without decryption to an XMP document.
fn valid_plaintext_metadata(ctx: &mut Context, sd: &StreamDict, cancel: &AtomicBool) -> bool {
    let mut probe = sd.clone();
    probe.content = None;
    if ctx.xref.decode_stream(&mut probe, cancel).is_err() {
        return false;
    }
    probe.content.as_deref().is_some_and(is_xmp_document)
}

/// `decryptStreamContent`: decrypts `sd.raw` by the crypt method of the stream.
fn decrypt_stream_content(
    ctx: &mut Context,
    sd: &mut StreamDict,
    obj_nr: i64,
    gen_nr: i64,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let metadata = metadata_stream(ctx, sd)?;
    if ctx.xref.enc_key.is_empty() {
        return Ok(());
    }
    // Unencrypted metadata.
    if let Some(e) = &ctx.xref.e
        && !e.emd
        && metadata
    {
        return Ok(());
    }

    let (encrypted, aes) = stream_crypt_method(&mut ctx.xref, sd)?;
    if !encrypted {
        return Ok(());
    }

    let (Some(raw), Some(e)) = (&sd.raw, &ctx.xref.e) else {
        return Err(Error::Panic("nil pointer dereference"));
    };
    match decrypt_stream(
        raw,
        obj_nr,
        gen_nr,
        &ctx.xref.enc_key,
        aes,
        e.r,
        ctx.xref.mem(),
    ) {
        Ok(raw) => {
            sd.raw = Some(raw);
            ensure_stream_length(sd, true);
            Ok(())
        }
        Err(err) => {
            if repairable_plaintext_metadata(ctx, metadata, &err)
                && valid_plaintext_metadata(ctx, sd, cancel)
            {
                return Ok(());
            }
            Err(err)
        }
    }
}

/// `isImageStreamDict`: `/Type /XObject` and `/Subtype /Image`, dereferenced.
fn is_image_stream_dict(ctx: &mut Context, sd: &StreamDict) -> Result<bool, Error> {
    let (t, _) = ctx
        .xref
        .dereference_name_entry(&sd.dict, b"Type")
        .map_err(|e| e.context("stream dict Type"))?;
    if t.as_deref() != Some(b"XObject".as_slice()) {
        return Ok(false);
    }
    let (subtype, _) = ctx
        .xref
        .dereference_name_entry(&sd.dict, b"Subtype")
        .map_err(|e| e.context("stream dict Subtype"))?;
    Ok(subtype.as_deref() == Some(b"Image".as_slice()))
}

/// `saveDecodedStreamContent`: decrypts the raw content of `sd` and, if `decode`, decodes it to
/// `sd.content`. Images stay encoded, and an unsupported filter is not an error.
///
/// # Errors
/// The decryption errors, the decode errors but an unsupported filter.
pub fn save_decoded_stream_content(
    rcx: &ReadCx<'_>,
    ctx: &mut Context,
    sd: &mut StreamDict,
    obj_nr: i64,
    gen_nr: i64,
    decode: bool,
) -> Result<(), Error> {
    // Special case: If the length of the encoded data is 0, we do not need to decode anything.
    if sd.raw.as_ref().is_none_or(|r| r.is_empty()) {
        sd.content = sd.raw.clone();
        return Ok(());
    }

    // XRefStreams are not encrypted.
    decrypt_stream_content(ctx, sd, obj_nr, gen_nr, rcx.cancel)
        .map_err(|e| e.context("decrypt stream content"))?;

    // A sole Crypt filter needs no further decoding once its crypt method has been applied.
    if !ctx.xref.enc_key.is_empty() && sd.has_sole_filter_named(b"Crypt") {
        sd.content = sd.raw.clone();
        return Ok(());
    }

    if !decode {
        return Ok(());
    }

    if is_image_stream_dict(ctx, sd).map_err(|e| e.context("classify image stream"))? {
        return Ok(());
    }

    // Actual decoding of stream data.
    match ctx.xref.decode_stream(sd, rcx.cancel) {
        Ok(()) => Ok(()),
        Err(e) if matches!(e.root(), Error::UnsupportedFilter(_)) => Ok(()),
        Err(e) => Err(e.context("decode stream content")),
    }
}

/// `loadStreamDict` (read.go:3891): loads and, if configured, decodes the content of `sd`, and
/// leaves `sd.stream_length` set. Streams are not decoded on read (`DecodeAllStreams` is off).
///
/// # Errors
/// The errors of [`load_encoded_stream_content`] and [`save_decoded_stream_content`].
pub fn load_stream_dict(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    sd: &mut StreamDict,
    obj_nr: i64,
    gen_nr: i64,
    fix_length: bool,
) -> Result<(), Error> {
    // Load encoded stream content for stream dicts into xRefTable entry.
    load_encoded_stream_content(rcx, ctx, sd, fix_length)
        .map_err(|e| e.context(format!("stream obj#{obj_nr}: load encoded content")))?;

    // Decode stream content.
    save_decoded_stream_content(rcx, ctx, sd, obj_nr, gen_nr, false)
        .map_err(|e| e.context(format!("stream obj#{obj_nr}: decode content")))
}

// ---------------------------------------------------------------------------------------------
// XMP

/// The part of `xml.Unmarshal(content, &XMPMeta{})` that decides success: valid UTF-8, a first
/// element `adobe:ns:meta/ xmpmeta`, and well-formed nesting through its end tag. Entities,
/// attribute syntax and encodings other than UTF-8 are not checked (docs/scope.md section 7 says
/// a minimal well-formedness check is enough).
fn is_xmp_document(content: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(content) else {
        return false;
    };
    let mut rest = text;
    let mut stack: Vec<&str> = Vec::new();
    let mut root_seen = false;

    // `rest` after the first `end` in it, or the empty string if there is none.
    fn after<'a>(rest: &'a str, end: &str) -> Option<&'a str> {
        rest.find(end).map(|i| &rest[i + end.len()..])
    }

    while let Some(lt) = rest.find('<') {
        rest = &rest[lt + 1..];
        let next = if let Some(r) = rest.strip_prefix('?') {
            after(r, "?>")
        } else if let Some(r) = rest.strip_prefix("!--") {
            after(r, "-->")
        } else if let Some(r) = rest.strip_prefix("![CDATA[") {
            after(r, "]]>")
        } else if let Some(r) = rest.strip_prefix('!') {
            after(r, ">")
        } else if let Some(r) = rest.strip_prefix('/') {
            let Some(gt) = r.find('>') else {
                return false;
            };
            let name = r[..gt].trim_end();
            if stack.pop() != Some(name) {
                return false;
            }
            if stack.is_empty() {
                // The root element is closed: Unmarshal stops there.
                return root_seen;
            }
            Some(&r[gt + 1..])
        } else {
            // A start tag: the name, then attributes up to `>` outside quotes.
            let name_end = rest
                .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .unwrap_or(rest.len());
            let name = &rest[..name_end];
            if name.is_empty() {
                return false;
            }
            let mut quote: Option<char> = None;
            let mut end = None;
            for (i, c) in rest[name_end..].char_indices() {
                match (quote, c) {
                    (Some(q), c) if c == q => quote = None,
                    (Some(_), _) => {}
                    (None, '"' | '\'') => quote = Some(c),
                    (None, '>') => {
                        end = Some(name_end + i);
                        break;
                    }
                    _ => {}
                }
            }
            let Some(end) = end else {
                return false;
            };
            let attrs = &rest[name_end..end];
            if !root_seen {
                root_seen = true;
                if !is_xmpmeta(name, attrs) {
                    return false;
                }
            }
            if !attrs.trim_end().ends_with('/') {
                stack.push(name);
            } else if stack.is_empty() {
                // A self-closing root.
                return true;
            }
            Some(&rest[end + 1..])
        };
        match next {
            Some(n) => rest = n,
            None => return false,
        }
    }
    false
}

/// The start tag `name attrs` is `adobe:ns:meta/ xmpmeta`: the local name, and the namespace the
/// element's prefix (or the default namespace) is bound to.
fn is_xmpmeta(name: &str, attrs: &str) -> bool {
    const NS: &str = "adobe:ns:meta/";
    let (prefix, local) = match name.split_once(':') {
        Some((p, l)) => (Some(p), l),
        None => (None, name),
    };
    if local != "xmpmeta" {
        return false;
    }
    let decl = match prefix {
        Some(p) => format!("xmlns:{p}"),
        None => "xmlns".to_string(),
    };
    for quote in ['"', '\''] {
        for sep in ["=", " =", "= ", " = "] {
            if attrs.contains(&format!("{decl}{sep}{quote}{NS}{quote}")) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::atomic::AtomicBool;

    fn ctx_for(input: &[u8]) -> Context {
        match Context::new(input.len() as u64, Config::default()) {
            Ok(c) => c,
            Err(e) => panic!("context: {e}"),
        }
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn name(s: &str) -> Object {
        Object::Name(s.as_bytes().to_vec())
    }

    // Port of TestReadStreamContentRejectsStreamLimit.
    #[test]
    fn read_stream_content_rejects_stream_limit() {
        let mem = Mem::with_max(1 << 20);
        let mut rd = PosReader::at(b"abc", 0).unwrap();
        let err = read_stream_content(&mut rd, &mem, 3, 3, 2).unwrap_err();
        assert!(err.to_string().contains("exceeds maximum"), "{err}");
    }

    // Port of TestReadStreamContentAcceptsWrappedEOF: the input ends before the stream length,
    // and the marker is found.
    #[test]
    fn read_stream_content_accepts_eof_after_marker() {
        let mem = Mem::with_max(1 << 20);
        let data = b"abc\nendstream\n";
        let mut rd = PosReader::at(data, 0).unwrap();
        let buf = read_stream_content(&mut rd, &mem, 20, 14, 20).unwrap();
        assert_eq!(&buf[..], b"abc\n");

        // No marker: the end of the input is an error.
        let mut rd = PosReader::at(b"abcdef", 0).unwrap();
        let err = read_stream_content(&mut rd, &mem, 20, 6, 20).unwrap_err();
        assert!(err.is(Sentinel::Eof));
    }

    #[test]
    fn read_stream_content_known_length_and_blind() {
        let mem = Mem::with_max(1 << 20);
        let data = b"hello world\nendstream\nendobj";
        let mut rd = PosReader::at(data, 0).unwrap();
        let buf = read_stream_content(&mut rd, &mem, 5, 28, 100).unwrap();
        assert_eq!((&buf[..], buf.go_cap()), (b"hello".as_slice(), 5));
        assert_eq!(mem.used(), 5);
        drop(buf);
        assert_eq!(mem.used(), 0);

        // No length: up to endstream, without the end of line.
        let mut rd = PosReader::at(data, 0).unwrap();
        let buf = read_stream_content(&mut rd, &mem, 0, 28, 100).unwrap();
        assert_eq!(&buf[..], b"hello world");

        // No marker within the limit.
        let mut rd = PosReader::at(b"no marker here at all", 0).unwrap();
        let err = read_stream_content(&mut rd, &mem, 0, 21, 8).unwrap_err();
        assert!(
            err.to_string().contains("maximum blind read length"),
            "{err}"
        );
    }

    #[test]
    fn ensure_stream_length_updates_dict() {
        let mem = Mem::with_max(100);
        let mut sd = StreamDict {
            raw: Some(mem.bytes_from(b"abc", 3).unwrap()),
            ..StreamDict::default()
        };
        ensure_stream_length(&mut sd, false);
        assert_eq!(sd.stream_length, Some(3));
        assert!(matches!(sd.dict.find(b"Length"), Some(Object::Integer(3))));

        // A matching length is kept unless fixing.
        sd.dict.set(b"Length", Object::Integer(99));
        ensure_stream_length(&mut sd, false);
        assert!(matches!(sd.dict.find(b"Length"), Some(Object::Integer(99))));
        ensure_stream_length(&mut sd, true);
        assert!(matches!(sd.dict.find(b"Length"), Some(Object::Integer(3))));
    }

    // Port of TestFilterPipelineClassifiesCorruptFilterArray.
    #[test]
    fn filter_pipeline_classifies_corrupt_filter_array() {
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(b"");
        let mut rcx = ReadCx::new(b"", &cancel, i64::MAX);

        let mut d = dict(&[("Filter", Object::Integer(1))]);
        let err = pdf_filter_pipeline(&mut rcx, &mut ctx, &mut d).unwrap_err();
        assert!(err.to_string().contains("filter pipeline"), "{err}");
        assert!(err.to_string().contains("corrupt filter array"), "{err}");

        let arr: Array = vec![Object::Integer(1)];
        let err = filter_names(&mut rcx, &mut ctx, &arr).unwrap_err();
        assert!(err.to_string().contains("entry 0"), "{err}");
    }

    // Port of TestFilterPipelineClassifiesCorruptDecodeParms.
    #[test]
    fn filter_pipeline_classifies_corrupt_decode_parms() {
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(b"");
        let mut rcx = ReadCx::new(b"", &cancel, i64::MAX);

        let d = dict(&[(
            "DecodeParms",
            Object::Array(vec![
                Object::Dict(Dict::default()),
                Object::Dict(Dict::default()),
            ]),
        )]);
        let err = single_filter(&mut rcx, &mut ctx, filter::FLATE, &d).unwrap_err();
        assert!(err.to_string().contains("decode parameters"), "{err}");
        assert!(err.to_string().contains("FlateDecode"), "{err}");

        let parms: Array = vec![Object::Integer(1)];
        let mut p = vec![PdfFilter {
            name: filter::FLATE.to_vec(),
            decode_parms: None,
        }];
        let err = build_filter_pipeline(&mut rcx, &mut ctx, &mut p, Some(&parms)).unwrap_err();
        assert!(err.to_string().contains("entry 0"), "{err}");
    }

    // Port of TestFilterPipelineNormalizesAliasesInRelaxedMode.
    #[test]
    fn filter_pipeline_normalizes_aliases() {
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(b"");
        let mut rcx = ReadCx::new(b"", &cancel, i64::MAX);

        let mut d = dict(&[("Filter", name("A85"))]);
        let pipeline = pdf_filter_pipeline(&mut rcx, &mut ctx, &mut d).unwrap();
        assert_eq!(pipeline[0].name, filter::ASCII85);
        assert_eq!(d.name_entry(b"Filter"), Some(filter::ASCII85));

        let aliases = ["AHx", "A85", "LZW", "Fl", "RL", "CCF", "DCT"];
        let want: [&[u8]; 7] = [
            filter::ASCII_HEX,
            filter::ASCII85,
            filter::LZW,
            filter::FLATE,
            filter::RUN_LENGTH,
            filter::CCITT_FAX,
            filter::DCT,
        ];
        let mut d = dict(&[(
            "Filter",
            Object::Array(aliases.iter().map(|a| name(a)).collect()),
        )]);
        let pipeline = pdf_filter_pipeline(&mut rcx, &mut ctx, &mut d).unwrap();
        for (i, f) in pipeline.iter().enumerate() {
            assert_eq!(f.name, want[i], "filter {i}");
        }
        let Some(Object::Array(arr)) = d.find(b"Filter") else {
            panic!("no filter array");
        };
        for (i, o) in arr.iter().enumerate() {
            assert!(matches!(o, Object::Name(n) if n == want[i]), "entry {i}");
        }
    }

    #[test]
    fn filter_pipeline_decode_parms() {
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(b"");
        let mut rcx = ReadCx::new(b"", &cancel, i64::MAX);

        // A single filter with a parameter dictionary, and an empty one.
        let parms = dict(&[("Predictor", Object::Integer(12))]);
        let mut d = dict(&[
            ("Filter", name("FlateDecode")),
            ("DecodeParms", Object::Dict(parms)),
        ]);
        let p = pdf_filter_pipeline(&mut rcx, &mut ctx, &mut d).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].decode_parms.as_ref().map(Dict::len), Some(1));

        let mut d = dict(&[
            ("Filter", name("FlateDecode")),
            ("DecodeParms", Object::Dict(Dict::default())),
        ]);
        let p = pdf_filter_pipeline(&mut rcx, &mut ctx, &mut d).unwrap();
        assert!(p[0].decode_parms.is_none());

        // An array of one, as some writers do; a null entry is no parameters.
        let mut d = dict(&[
            ("Filter", name("FlateDecode")),
            ("DecodeParms", Object::Array(vec![Object::Null])),
        ]);
        let p = pdf_filter_pipeline(&mut rcx, &mut ctx, &mut d).unwrap();
        assert!(p[0].decode_parms.is_none());

        // A pipeline whose parameter array has the wrong length.
        let mut d = dict(&[
            (
                "Filter",
                Object::Array(vec![name("ASCIIHexDecode"), name("FlateDecode")]),
            ),
            ("DecodeParms", Object::Array(vec![Object::Null])),
        ]);
        assert!(pdf_filter_pipeline(&mut rcx, &mut ctx, &mut d).is_err());

        // No filter, no pipeline.
        let mut d = Dict::default();
        assert!(
            pdf_filter_pipeline(&mut rcx, &mut ctx, &mut d)
                .unwrap()
                .is_empty()
        );
    }

    // Port of TestSaveDecodedStreamContentIgnoresUnsupportedFilter.
    #[test]
    fn save_decoded_stream_content_ignores_unsupported_filter() {
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(b"");
        let rcx = ReadCx::new(b"", &cancel, i64::MAX);
        let mut sd = StreamDict {
            raw: Some(ctx.xref.mem().bytes_from(b"abc", 3).unwrap()),
            filter_pipeline: vec![PdfFilter {
                name: filter::JBIG2.to_vec(),
                decode_parms: None,
            }],
            ..StreamDict::default()
        };
        save_decoded_stream_content(&rcx, &mut ctx, &mut sd, 0, 0, true).unwrap();
    }

    #[test]
    fn save_decoded_stream_content_decodes() {
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(b"");
        let rcx = ReadCx::new(b"", &cancel, i64::MAX);
        let mut sd = StreamDict {
            raw: Some(ctx.xref.mem().bytes_from(b"48656c6c6f>", 11).unwrap()),
            filter_pipeline: vec![PdfFilter {
                name: filter::ASCII_HEX.to_vec(),
                decode_parms: None,
            }],
            ..StreamDict::default()
        };

        // Not decoded unless asked to.
        save_decoded_stream_content(&rcx, &mut ctx, &mut sd, 1, 0, false).unwrap();
        assert!(sd.content.is_none());
        save_decoded_stream_content(&rcx, &mut ctx, &mut sd, 1, 0, true).unwrap();
        assert_eq!(sd.content.as_deref(), Some(b"Hello".as_slice()));

        // An image stays encoded.
        let mut img = StreamDict {
            dict: dict(&[("Type", name("XObject")), ("Subtype", name("Image"))]),
            raw: sd.raw.clone(),
            filter_pipeline: sd.filter_pipeline.clone(),
            ..StreamDict::default()
        };
        save_decoded_stream_content(&rcx, &mut ctx, &mut img, 2, 0, true).unwrap();
        assert!(img.content.is_none());

        // An empty stream has the (empty) raw buffer as its content.
        let mut empty = StreamDict {
            raw: Some(ctx.xref.mem().bytes_from(b"", 0).unwrap()),
            ..StreamDict::default()
        };
        save_decoded_stream_content(&rcx, &mut ctx, &mut empty, 3, 0, true).unwrap();
        assert!(empty.content.is_some());
    }

    #[test]
    fn load_encoded_stream_content_with_length_and_fix() {
        let data = b"1 0 obj\n<< /Length 3 >>\nstream\nabcdef\nendstream\nendobj\n";
        let off = data.windows(6).position(|w| w == b"stream").unwrap() as i64 + 7;
        let cancel = AtomicBool::new(false);
        let mut ctx = ctx_for(data);
        let mut rcx = ReadCx::new(data, &cancel, i64::MAX);

        let mut sd = StreamDict::new(
            dict(&[("Length", Object::Integer(3))]),
            off,
            Some(3),
            None,
            vec![],
        );
        load_encoded_stream_content(&mut rcx, &mut ctx, &mut sd, false).unwrap();
        assert_eq!(sd.raw.as_deref(), Some(b"abc".as_slice()));
        assert_eq!(sd.stream_length, Some(3));

        // Fixing a wrong length reads up to endstream.
        let mut sd = StreamDict::new(
            dict(&[("Length", Object::Integer(3))]),
            off,
            Some(3),
            None,
            vec![],
        );
        load_encoded_stream_content(&mut rcx, &mut ctx, &mut sd, true).unwrap();
        assert_eq!(sd.raw.as_deref(), Some(b"abcdef".as_slice()));
        assert!(matches!(sd.dict.find(b"Length"), Some(Object::Integer(6))));

        // A stream that is loaded already is left alone.
        let before = sd.raw.clone();
        load_encoded_stream_content(&mut rcx, &mut ctx, &mut sd, true).unwrap();
        assert!(Bytes::ptr_eq(&before.unwrap(), sd.raw.as_ref().unwrap()));
    }

    #[test]
    fn load_encoded_stream_content_counts_the_stream_budget() {
        let data = b"abcdefghij";
        let cancel = AtomicBool::new(false);
        let mut conf = Config::default();
        conf.limits.max_stream_total_bytes = 8;
        let mut ctx = Context::new(data.len() as u64, conf).unwrap();
        let mut rcx = ReadCx::new(data, &cancel, i64::MAX);

        let mut sd = StreamDict::new(Dict::default(), 0, Some(5), None, vec![]);
        load_encoded_stream_content(&mut rcx, &mut ctx, &mut sd, false).unwrap();
        let mut sd2 = StreamDict::new(Dict::default(), 0, Some(5), None, vec![]);
        let err = load_encoded_stream_content(&mut rcx, &mut ctx, &mut sd2, false).unwrap_err();
        assert!(err.is_limit_exceeded(), "{err}");
    }

    #[test]
    fn xmp_check() {
        let ok = br#"<?xpacket begin="" id="x"?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description/></rdf:RDF></x:xmpmeta><?xpacket end="w"?>"#;
        assert!(is_xmp_document(ok));
        // The default namespace works as well.
        assert!(is_xmp_document(
            br#"<xmpmeta xmlns="adobe:ns:meta/"></xmpmeta>"#
        ));
        // Wrong namespace, wrong root, unclosed, empty, not UTF-8.
        assert!(!is_xmp_document(
            br#"<x:xmpmeta xmlns:x="other:"></x:xmpmeta>"#
        ));
        assert!(!is_xmp_document(br#"<a></a>"#));
        assert!(!is_xmp_document(
            br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><a>"#
        ));
        assert!(!is_xmp_document(
            br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><a></b></x:xmpmeta>"#
        ));
        assert!(!is_xmp_document(b""));
        assert!(!is_xmp_document(b"\xff\xfe<x/>"));
    }
}
