//! The header version, the repair scanner, and the loop that builds the cross-reference table
//! (Go: pkg/pdfcpu/read.go:1626-2290): `header_version`, `bypass_xref_section` (which rebuilds
//! the table from the objects it finds), `build_xref_table_starting_at` (follows the `Prev`
//! chain) and `read_xref_table`.

use super::xref::{
    offset_last_xref_section, parse_xref_section, process_trailer, scan_line, scan_line_raw,
};
use super::xrefstm::parse_xref_stream;
use super::{
    ChargedBuf, ObjFail, PosReader, ReadCx, contains, dereferenced_object, err_corrupt_header,
    err_postscript_input, find, find_any, go_fields, go_trim_space, has_post_script_header,
    is_missing_scanner_line, new_repair_scanner, object, read_info, resolve_object, stream, to_i64,
    trim_left,
};
use crate::error::{Error, Sentinel};
use crate::model::{Context, Entry, EntryObject, Version, XRefTable};
use crate::object::string::trim_leading_comment;
use crate::object::{Dict, Object, Ref, dict_copy_bytes};
use crate::parse::parse_object_attributes;
use crate::parse::scan::{Scanner, lines};
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------------------------
// The header

/// `scanForVersion`: finds the first `prefix` and returns at least 18 bytes from its start,
/// plus its file offset. The bytes are those of the 100-byte blocks read up to the one that
/// holds the end of the prefix (and further blocks while there are fewer than 18).
fn scan_for_version(data: &[u8], prefix: &[u8]) -> Result<(Vec<u8>, usize), Error> {
    const BUF_SIZE: usize = 100;
    const HEADER_SCAN_LOOKAHEAD: usize = 18;

    let mut rd = PosReader::at(data, 0)?;

    // `buf` holds the bytes not yet ruled out, starting at file offset `off`. Between blocks
    // only a tail shorter than prefix is carried over, so a prefix spanning two blocks is
    // still found and every candidate position is checked exactly once.
    let mut buf: Vec<u8> = Vec::new();
    let mut off = 0usize;
    let mut first = true;

    loop {
        let chunk = match rd.fill(BUF_SIZE) {
            Ok(c) => c,
            Err(e) if e.is(Sentinel::Eof) => return Err(err_corrupt_header()),
            Err(e) => return Err(e.context("read header")),
        };
        if first && has_post_script_header(chunk) {
            return Err(err_postscript_input());
        }
        first = false;
        buf.try_reserve(chunk.len())?;
        buf.extend_from_slice(chunk);

        let Some(i) = find(&buf, prefix) else {
            let keep = (prefix.len() - 1).min(buf.len());
            let skip = buf.len() - keep;
            off += skip;
            buf.drain(..skip);
            continue;
        };

        off += i;
        buf.drain(..i);
        while buf.len() < HEADER_SCAN_LOOKAHEAD {
            let chunk = match rd.fill(BUF_SIZE) {
                Ok(c) => c,
                Err(e) if e.is(Sentinel::Eof) => return Err(err_corrupt_header()),
                Err(e) => return Err(e.context("read header")),
            };
            buf.try_reserve(chunk.len())?;
            buf.extend_from_slice(chunk);
        }
        return Ok((buf, off));
    }
}

/// `headerVersion`: the version of the first line of the file, the number of characters of its
/// end of line (1 or 2), and the offset of the header.
///
/// Beginning with PDF 1.4, the `Version` entry in the document's catalog dictionary (located
/// via the `Root` entry in the file's trailer, as described in 7.5.5, "File Trailer"), if
/// present, shall be used instead of the version specified in the header.
///
/// # Errors
/// [`err_corrupt_header`], [`err_postscript_input`], an unknown version.
pub fn header_version(data: &[u8]) -> Result<(Version, i64, i64), Error> {
    const PREFIX: &[u8] = b"%PDF-";

    let (s, off) = scan_for_version(data, PREFIX)?;

    let version = s
        .get(PREFIX.len()..PREFIX.len() + 3)
        .ok_or_else(err_corrupt_header)
        .and_then(Version::parse)
        .map_err(|e| e.context("unknown PDF header version"))?;

    let s = s.get(8..).ok_or_else(err_corrupt_header)?;
    let s = trim_left(s, b"\t\x0c ");

    // Detect the used eol which should be 1 (0x00, 0x0D) or 2 chars (0x0D0A) long.
    // %PDF-1.x{whiteSpace}{text}{eol} or
    let j = find_any(s, b"\x0A\x0D").ok_or_else(err_corrupt_header)?;

    let eol_count = if s[j] == 0x0D && s.get(j + 1) == Some(&0x0A) {
        2
    } else {
        1
    };

    Ok((version, eol_count, to_i64(off)))
}

// ---------------------------------------------------------------------------------------------
// Repair

/// `isCatalogForRepair`: whether the dict `d` has the `Type` `Catalog`.
fn is_catalog_for_repair(rcx: &mut ReadCx<'_>, ctx: &mut Context, d: &Dict) -> Result<bool, Error> {
    let o = match d.find(b"Type") {
        None | Some(Object::Null) => return Ok(false),
        Some(o) => o,
    };

    if let Object::Ref(ir) = o {
        let Some(entry) = ctx.xref.find_table_entry_for_ind_ref(*ir) else {
            return Ok(false);
        };
        if entry.free || entry.compressed {
            return Ok(false);
        }
        if entry.object.is_none() && entry.offset.is_some() {
            match dereferenced_object(rcx, ctx, ir.num) {
                Ok(_) => {}
                Err(e) if e.is(Sentinel::NilDereferenced) => return Ok(false),
                Err(e) => return Err(e.context(format!("obj#{}", ir.num))),
            }
        }
    } else if !matches!(o, Object::Name(_)) {
        return Ok(false);
    }

    let (t, _) = ctx.xref.dereference_name_entry(d, b"Type")?;
    Ok(t.as_deref() == Some(b"Catalog".as_slice()))
}

/// `parseAndLoad`: parses the object that `line` starts, at `offset`, into its table entry
/// (creating the entry if there is none), and moves `offset` behind the line (behind the stream
/// data of a stream). A catalog becomes the root.
///
/// # Errors
/// The errors of `object`, `resolve_object` and the stream load, with the `end_ind` Go returns
/// beside them (0 where it returns the zero value).
pub fn parse_and_load(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    line: &[u8],
    offset: &mut i64,
    incr: i64,
) -> Result<(), ObjFail> {
    let mut l = line;
    let (obj_nr, generation) = parse_object_attributes(&mut l)?;

    if obj_nr == 0 {
        return Ok(());
    }

    let off = *offset;

    let out = object(rcx, ctx, off)?;
    let end_ind = out.end_ind;
    let fail = |err: Error| ObjFail { end_ind, err };
    let mut obj = resolve_object(
        rcx,
        ctx,
        out.obj,
        off,
        obj_nr,
        generation,
        out.end_ind,
        out.stream_ind,
        out.stream_offset,
    )
    .map_err(fail)?;

    if let Object::Dict(d) = &obj {
        let catalog = is_catalog_for_repair(rcx, ctx, d)
            .map_err(|e| fail(e.context(format!("repair catalog candidate obj#{obj_nr} Type"))))?;
        if catalog {
            // The copy is charged until it drops. An allocation failure is not a malformed
            // object: it is never skipped.
            ctx.xref
                .held
                .grow_by(dict_copy_bytes(d))
                .map_err(|err| ObjFail { end_ind: -1, err })?;
            if let Some(old) = ctx.xref.root_dict.replace(d.clone()) {
                release(&mut ctx.xref, &old);
            }
            ctx.xref.root = Some(Ref {
                num: obj_nr,
                generation,
            });
        }
    }

    *offset = offset.wrapping_add(to_i64(line.len()));

    if let Object::Stream(sd) = &mut obj {
        stream::load_stream_dict(rcx, ctx, sd, obj_nr, generation, true).map_err(fail)?;
        let len = sd
            .stream_length
            .ok_or(Error::Panic("nil stream length"))
            .map_err(fail)?;
        *offset = sd.stream_offset.wrapping_add(len);
    }

    // A null object is Go's nil: the entry stays unloaded.
    let entry_object = if obj.is_null() {
        None
    } else {
        Some(EntryObject::Obj(obj))
    };
    match ctx.xref.table.get_mut(&obj_nr) {
        None => {
            // An allocation failure is not a malformed object: it is never skipped.
            ctx.xref
                .insert_charged(
                    obj_nr,
                    Entry {
                        free: false,
                        generation: Some(generation),
                        offset: Some(off),
                        object: entry_object,
                        incr,
                        ..Entry::default()
                    },
                )
                .map_err(|err| ObjFail { end_ind: -1, err })?;
        }
        Some(e) => {
            e.offset = Some(off);
            e.object = entry_object;
            e.incr = incr;
        }
    }

    Ok(())
}

/// `processObject`: loads the object that `line` starts and returns a scanner that resumes at
/// `offset`, and whether it resumes before the current object's `endobj` marker. A malformed
/// object that has an `endobj` is skipped.
///
/// # Errors
/// The errors of [`parse_and_load`] when the object has no `endobj` in its buffer, and a
/// negative offset.
pub fn process_object<'a>(
    rcx: &mut ReadCx<'a>,
    ctx: &mut Context,
    line: &[u8],
    offset: &mut i64,
    incr: i64,
) -> Result<(Scanner<'a>, bool), Error> {
    let mut within_obj = true;
    if let Err(f) = parse_and_load(rcx, ctx, line, offset, incr) {
        if f.end_ind < 0 {
            return Err(f.err);
        }
        // Relaxed: skip the malformed object.
        *offset = offset.wrapping_add(f.end_ind + 6);
        within_obj = false;
    }
    let rd = PosReader::at(rcx.data, *offset)?;
    Ok((new_repair_scanner(&ctx.xref.conf.limits, rd), within_obj))
}

/// `objCandidate`: whether `line` may start an object (`n g obj`, not the end of one).
pub fn obj_candidate(within_obj: bool, line: &[u8]) -> bool {
    if within_obj {
        return false;
    }
    match find(line, b"obj") {
        Some(i) => i > 2 && find(line, b"endobj") != Some(i - 3),
        None => false,
    }
}

/// `checkEndObj`: inside an object, moves `line` to its `endobj` and leaves the object.
pub fn check_end_obj(within_obj: &mut bool, line: &mut &[u8]) {
    if *within_obj && let Some(i) = find(line, b"endobj") {
        *line = &line[i..];
        *within_obj = false;
    }
}

/// `ensureNoStartXRef`: whether the `xref` found at `i` is a keyword of its own, not the end of
/// `startxref`.
pub fn ensure_no_start_xref(line: &[u8], i: Option<usize>) -> bool {
    match i {
        Some(0) => true,
        Some(n) => line[n - 1] != b't',
        None => false,
    }
}

/// `registerXRefEntry`: a table entry for the object that `line` starts, at `offset`.
///
/// # Errors
/// [`Error::Alloc`] for the entry's charge.
fn register_xref_entry(
    ctx: &mut Context,
    line: &[u8],
    offset: i64,
    incr: i64,
) -> Result<bool, Error> {
    let mut l = line;
    let Ok((obj_nr, generation)) = parse_object_attributes(&mut l) else {
        return Ok(false);
    };
    if obj_nr == 0 {
        return Ok(false);
    }

    ctx.xref.insert_charged(
        obj_nr,
        Entry {
            generation: Some(generation),
            offset: Some(offset),
            incr,
            ..Entry::default()
        },
    )?;
    Ok(true)
}

/// `registerXRefObjects`: scans the whole input line by line and registers every object it
/// finds, by offset, in the table.
///
/// # Errors
/// The scanner's error (a line over the object buffer limit), [`Error::Cancelled`],
/// [`Error::Alloc`] for the entries.
pub fn register_xref_objects(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    incr: i64,
) -> Result<(), Error> {
    let rd = PosReader::at(rcx.data, 0)?;
    let mut s = new_repair_scanner(&ctx.xref.conf.limits, rd);

    let mut offset: i64 = 0;
    let mut within_obj = false;
    while s.scan() {
        rcx.check()?;
        let raw_line = s.bytes();
        let mut line = trim_leading_comment(raw_line);
        check_end_obj(&mut within_obj, &mut line);
        if obj_candidate(within_obj, line) && register_xref_entry(ctx, line, offset, incr)? {
            within_obj = !contains(line, b"endobj");
        }
        offset = offset.wrapping_add(to_i64(raw_line.len()) + 1);
    }
    if let Some(e) = s.err() {
        return Err(e.context("repair xref table: register objects"));
    }
    Ok(())
}

/// `xrefRepairState`.
struct XRefRepairState<'a> {
    scanner: Scanner<'a>,
    offset: i64,
    within_obj: bool,
    within_xref: bool,
    within_trailer: bool,
    bb: ChargedBuf,
}

/// `processXRefRepairLine`: while inside an `xref` section, follows it to its trailer and reads
/// that. Returns whether the line was part of the section.
fn process_xref_repair_line(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    state: &mut XRefRepairState<'_>,
    line: &[u8],
    length: usize,
    eol_count: i64,
    off_extra: i64,
    incr: i64,
) -> Result<bool, Error> {
    if !state.within_xref {
        return Ok(false);
    }

    state.offset = state
        .offset
        .wrapping_add(to_i64(length).wrapping_add(eol_count));
    if !state.within_trailer {
        if contains(line, b"trailer") {
            state.bb.clear();
            state.bb.extend(line)?;
            state.within_trailer = true;
        }
        return Ok(true);
    }
    if length == 0 {
        return Ok(true);
    }

    state.bb.push(b'\n')?;
    state.bb.extend(line)?;
    if !line.starts_with(b"startxref") {
        return Ok(true);
    }

    process_trailer(
        rcx,
        ctx,
        &mut state.scanner,
        &state.bb.v,
        None,
        off_extra,
        incr,
        true,
    )?;
    state.within_xref = false;
    state.within_trailer = false;
    Ok(true)
}

/// `discardPreEncryptionObjects`: after a repair of an encrypted file that has no key yet, the
/// objects that were read before the encryption dictionary was set up are dropped, so they are
/// read again with decryption.
pub fn discard_pre_encryption_objects(ctx: &mut Context) {
    let x = &mut ctx.xref;
    if x.encrypt.is_none() || !x.enc_key.is_empty() {
        return;
    }

    for entry in x.table.values_mut() {
        if entry.free || entry.compressed || entry.offset.is_none() {
            continue;
        }
        entry.object = None;
    }

    if let Some(rd) = x.root_dict.take() {
        release(x, &rd);
    }
}

/// `bypassXrefSection`: a fix for digesting corrupt xref sections. It populates the table by
/// reading in all indirect objects line by line, and works on the assumption of a single xref
/// section, meaning no incremental updates.
///
/// # Errors
/// `was_err` itself if it is a decode limit error, the scanner errors, the errors of
/// [`process_object`] that are not skipped, [`Error::Cancelled`].
pub fn bypass_xref_section<'a>(
    rcx: &mut ReadCx<'a>,
    ctx: &mut Context,
    off_extra: i64,
    was_err: &Error,
    incr: i64,
) -> Result<(), Error> {
    if was_err.is_decode_limit_exceeded() {
        return Err(was_err.clone());
    }
    ctx.xref.table.clear();
    ctx.xref.table.insert(0, Entry::free_head());
    register_xref_objects(rcx, ctx, incr)?;

    let mem = ctx.xref.mem().clone();
    let rd = PosReader::at(rcx.data, 0)?;
    let eol_count: i64 = 1;

    let mut state = XRefRepairState {
        scanner: new_repair_scanner(&ctx.xref.conf.limits, rd),
        offset: 0,
        within_obj: false,
        within_xref: false,
        within_trailer: false,
        bb: ChargedBuf::new(&mem)?,
    };
    let mut prev_line = ChargedBuf::new(&mem)?;

    loop {
        rcx.check()?;
        let raw = match scan_line_raw(&mut state.scanner) {
            Ok(l) => l,
            Err(e) if is_missing_scanner_line(&e) => break,
            Err(e) => return Err(e.context("repair xref table: scan input")),
        };
        let length = raw.len();
        let trimmed = trim_leading_comment(raw);
        let joined: ChargedBuf;
        let mut line: &[u8] = if prev_line.v.is_empty() {
            trimmed
        } else {
            let mut j = ChargedBuf::new(&mem)?;
            j.extend(&prev_line.v)?;
            j.extend(trimmed)?;
            prev_line.clear();
            joined = j;
            joined.v.as_slice()
        };

        if process_xref_repair_line(
            rcx, ctx, &mut state, line, length, eol_count, off_extra, incr,
        )? {
            continue;
        }
        let i = find(line, b"xref");
        if ensure_no_start_xref(line, i) {
            state.offset = state.offset.wrapping_add(to_i64(length) + eol_count);
            state.within_xref = true;
            continue;
        }
        check_end_obj(&mut state.within_obj, &mut line);
        if obj_candidate(state.within_obj, line) {
            if !line.ends_with(b"obj") {
                let (scanner, within_obj) =
                    process_object(rcx, ctx, line, &mut state.offset, incr)?;
                state.scanner = scanner;
                state.within_obj = within_obj;
                continue;
            }
            prev_line.extend(line)?;
            continue;
        }

        state.offset = state.offset.wrapping_add(to_i64(length) + eol_count);
    }
    discard_pre_encryption_objects(ctx);
    unalias_repaired_catalog(&mut ctx.xref);
    Ok(())
}

/// Go's repaired `RootDict` (read.go:1760) is the map stored in the table, so changes made
/// through the catalog show in the catalog object. While the table still holds that catalog,
/// drop the copy so that `catalog_mut` changes the table entry.
fn unalias_repaired_catalog(x: &mut XRefTable) {
    let (Some(rd), Some(root)) = (&x.root_dict, x.root) else {
        return;
    };
    let same = matches!(x.table.get(&root.num), Some(Entry {
        object: Some(EntryObject::Obj(Object::Dict(d))),
        ..
    }) if same_dict(d, rd));
    if same && let Some(rd) = x.root_dict.take() {
        release(x, &rd);
    }
}

/// Releases the charge of `root_dict`, a copy that has dropped out of the table.
fn release(x: &mut XRefTable, root_dict: &Dict) {
    let left = x.held.bytes().saturating_sub(dict_copy_bytes(root_dict));
    x.held.shrink_to(left);
}

/// Whether `a` and `b` are the same dictionary, entry by entry, without allocating. Floats are
/// compared by their bits, and streams (which a parsed dictionary does not hold) are never the
/// same.
fn same_dict(a: &Dict, b: &Dict) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|((ka, va), (kb, vb))| ka == kb && same_object(va, vb))
}

fn same_object(a: &Object, b: &Object) -> bool {
    match (a, b) {
        (Object::Null, Object::Null) => true,
        (Object::Bool(x), Object::Bool(y)) => x == y,
        (Object::Integer(x), Object::Integer(y)) => x == y,
        (Object::Float(x), Object::Float(y)) => x.to_bits() == y.to_bits(),
        (Object::Name(x), Object::Name(y))
        | (Object::String(x), Object::String(y))
        | (Object::Hex(x), Object::Hex(y)) => x == y,
        (Object::Ref(x), Object::Ref(y)) => x == y,
        (Object::Array(x), Object::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(u, v)| same_object(u, v))
        }
        (Object::Dict(x), Object::Dict(y)) => same_dict(x, y),
        _ => false,
    }
}

/// `postProcess`: ensures free object #0 if there was exactly one xref subsection and it left
/// no object 0, by shifting the parsed entries down by one when object 1 is free, else by
/// creating the head of the free list.
pub fn post_process(ctx: &mut Context, xref_section_count: i64) {
    if xref_section_count == 1 && !ctx.xref.exists(0) {
        // Fix for #250
        if ctx.xref.table.get(&1).is_some_and(|e| e.free) {
            // Create free object 0 by shifting down parsed objects by one.
            let old = std::mem::take(&mut ctx.xref.table);
            for (obj_nr, entry) in old {
                let to = if obj_nr == 0 { 0 } else { obj_nr - 1 };
                ctx.xref.table.insert(to, entry);
            }
        } else {
            // Create free object 0 from scratch if the free list head is missing.
            ctx.xref.table.insert(0, Entry::free_head());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The xref table

/// `tryXRefSection`: tries to read a classic xref section at `offset`. Returns the offset of
/// the previous section; `Some(0)` means the offset does not start a classic section and an
/// xref stream is next.
///
/// # Errors
/// The scanner and section errors.
pub fn try_xref_section(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    offset: i64,
    off_extra: i64,
    xref_section_count: &mut i64,
    incr: i64,
) -> Result<Option<i64>, Error> {
    let rd = PosReader::at(rcx.data, offset)?;
    let mut s = Scanner::with_buffer(rd.rest(), lines, 4096, 1024 * 1024);

    let line = scan_line(&mut s)?;
    let repair_off = line.len();

    if go_trim_space(line) == b"xref" {
        return parse_xref_section(
            rcx,
            ctx,
            &mut s,
            &[],
            xref_section_count,
            offset,
            off_extra,
            incr,
        );
    }

    // Repair fix for #823
    if line.starts_with(b"xref") {
        let fields = go_fields(line)?;
        if fields.len() == 3 {
            return parse_xref_section(
                rcx,
                ctx,
                &mut s,
                &fields[1..],
                xref_section_count,
                offset,
                off_extra,
                incr,
            );
        }
    }

    // Repair fix for #326
    let line = scan_line(&mut s)?;

    let i = find(line, b"xref");
    if ensure_no_start_xref(line, i) {
        // Don't confuse with "startxref".
        read_info(ctx)?.repair_offset = to_i64(repair_off + i.unwrap_or(0));
        return parse_xref_section(
            rcx,
            ctx,
            &mut s,
            &[],
            xref_section_count,
            offset,
            off_extra,
            incr,
        );
    }

    Ok(Some(0))
}

/// `parseXRefStreamOrRepair`: reads an xref stream at `offset`. If that fails, repairs the
/// table. Returns the offset of the previous section and whether the table was repaired.
///
/// # Errors
/// The errors of the repair, and a negative offset.
pub fn parse_xref_stream_or_repair(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    offset: &mut i64,
    off_extra: i64,
    incr: i64,
) -> Result<(Option<i64>, bool), Error> {
    read_info(ctx)?.using_xref_streams = true;
    let mut rd = PosReader::at(rcx.data, *offset)?;

    match parse_xref_stream(rcx, ctx, &mut rd, offset, off_extra, incr) {
        Ok(next) => Ok((next, false)),
        // Try fix for corrupt single xref section.
        Err(err) => {
            bypass_xref_section(rcx, ctx, off_extra, &err, incr)?;
            Ok((None, true))
        }
    }
}

/// `repairCorruptXRefSection`: a section of the first increment whose trailer is missing is
/// repaired; any other error stands.
fn repair_corrupt_xref_section(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    off_extra: i64,
    err: Error,
    incr: i64,
) -> Result<(), Error> {
    if incr != 1 || !err.is(Sentinel::MissingTrailerDict) {
        return Err(err);
    }
    bypass_xref_section(rcx, ctx, off_extra, &err, incr)
}

/// `buildXRefTableStartingAt`: builds the table by reading xref streams or sections, from
/// `offset` back along the `Prev` chain.
///
/// # Errors
/// The header, section, stream and repair errors, [`Error::Cancelled`].
pub fn build_xref_table_starting_at(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    offset: i64,
) -> Result<(), Error> {
    let (hv, eol_count, off_extra) = header_version(rcx.data)?;
    let file_size = to_i64(rcx.data.len());

    ctx.xref.header_version = Some(hv);
    read_info(ctx)?.eol_count = eol_count;
    let mut offs: BTreeSet<i64> = BTreeSet::new();
    let mut xref_section_count: i64 = 0;
    let mut incr: i64 = 0;

    let mut next = Some(offset.wrapping_add(off_extra));
    while let Some(first) = next {
        incr += 1;

        rcx.check()?;

        let mut cur = first;
        if offs.contains(&cur) {
            cur = offset_last_xref_section(rcx.data, file_size.wrapping_sub(cur))?;
            if offs.contains(&cur) {
                return Ok(());
            }
        }

        offs.insert(cur);

        let off = match try_xref_section(rcx, ctx, cur, off_extra, &mut xref_section_count, incr) {
            Ok(off) => off,
            Err(err) => return repair_corrupt_xref_section(rcx, ctx, off_extra, err, incr),
        };

        match off {
            None => {
                next = None;
                continue;
            }
            Some(o) if o != 0 => {
                next = Some(o);
                continue;
            }
            Some(_) => {}
        }

        let (following, repaired) =
            parse_xref_stream_or_repair(rcx, ctx, &mut cur, off_extra, incr)?;
        next = following;
        if repaired {
            return Ok(());
        }
    }

    post_process(ctx, xref_section_count);

    Ok(())
}

/// `repairableLastXRefSection`: whether the table can be repaired after a failed search for the
/// last xref section.
fn repairable_last_xref_section(err: &Error) -> bool {
    err.is(Sentinel::MissingXRefSection) || err.is(Sentinel::MissingXRefEof)
}

/// `readXRefTable`: populates the cross-reference table for this file. Goes to the offset of
/// the first xref section, which is `xref` or an indirect object like `34 0 obj`, and keeps
/// digesting sections as long as there is a previous one. Then repairs the free list.
///
/// # Errors
/// The errors of [`build_xref_table_starting_at`] and of the free list repair.
pub fn read_xref_table(rcx: &mut ReadCx<'_>, ctx: &mut Context) -> Result<(), Error> {
    let offset = match offset_last_xref_section(rcx.data, 0) {
        Ok(offset) => offset,
        Err(err) => {
            if !repairable_last_xref_section(&err) {
                return Err(err);
            }
            0
        }
    };

    match build_xref_table_starting_at(rcx, ctx, offset) {
        Ok(()) => {}
        Err(err) if err.is(Sentinel::Eof) => return Err(err.context("xref table: unexpected eof")),
        Err(err) => return Err(err),
    }

    // Note: Acrobat 6.0 and later do not use the free list to recycle object numbers - pdfcpu
    // does.
    ctx.xref.ensure_valid_free_list()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Limits};
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

    fn limit(max_object_bytes: u64) -> Limits {
        Limits {
            max_object_bytes,
            ..Limits::default()
        }
    }

    // Port of TestHeaderVersionAcrossBlockBoundaries.
    #[test]
    fn header_version_across_block_boundaries() {
        for version in [Version::V17, Version::V20] {
            for false_prefix in ["", "%", "%P", "%PD", "%%PDFX"] {
                for pad in 0..205 {
                    let input = format!(
                        "{}{false_prefix}%PDF-{version}\r\n{}",
                        "a".repeat(pad),
                        "b".repeat(200)
                    );
                    let (v, eol, off) = header_version(input.as_bytes())
                        .unwrap_or_else(|e| panic!("{version} {pad} {false_prefix:?}: {e}"));
                    assert_eq!(
                        (v, eol, off),
                        (version, 2, (pad + false_prefix.len()) as i64),
                        "{version} {pad} {false_prefix:?}"
                    );
                }
            }
        }
    }

    // Port of TestHeaderVersionShortLookahead.
    #[test]
    fn header_version_short_lookahead() {
        for pad in [0usize, 85, 95, 99, 100] {
            for size in [9usize, 17, 18, 19] {
                let input = format!("{}%PDF-1.7\n{}", "a".repeat(pad), "b".repeat(size - 9));
                let r = header_version(input.as_bytes());
                if size < 18 {
                    assert_eq!(r, Err(err_corrupt_header()), "padding {pad}, size {size}");
                    continue;
                }
                assert_eq!(
                    r,
                    Ok((Version::V17, 1, pad as i64)),
                    "padding {pad}, size {size}"
                );
            }
        }
    }

    // Port of TestHeaderVersionRejectsPostScript.
    #[test]
    fn header_version_rejects_post_script() {
        for input in [
            &b"%!PS-Adobe-3.0\n%%Title: report.txt\n"[..],
            &b"\xef\xbb\xbf \r\n%!PS-Adobe-3.0\n"[..],
        ] {
            assert_eq!(header_version(input), Err(err_postscript_input()));
        }
    }

    // Port of TestHeaderVersionClassifiesCorruptHeader.
    #[test]
    fn header_version_classifies_a_corrupt_header() {
        for input in [&b"not a pdf\n"[..], &b"%PDF-1.7"[..], &b""[..]] {
            assert_eq!(
                header_version(input),
                Err(err_corrupt_header()),
                "{input:?}"
            );
        }
    }

    // Port of TestHeaderVersionUnknownVersionIncludesContext.
    #[test]
    fn header_version_unknown_version_includes_context() {
        let err = header_version(b"%PDF-9.9\n% padding for header scan\n").unwrap_err();
        assert!(
            err.to_string().contains("unknown PDF header version"),
            "{err}"
        );
    }

    #[test]
    fn header_eol_variants() {
        assert_eq!(
            header_version(b"%PDF-1.4 text here\r\nxxxxxxxxxxxxxxxxxxxxxxx").map(|v| v.1),
            Ok(2)
        );
        assert_eq!(
            header_version(b"%PDF-1.4\rxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").map(|v| v.1),
            Ok(1)
        );
        assert_eq!(
            header_version(b"%PDF-1.4\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").map(|v| v.1),
            Ok(1)
        );
        // The end of the line must be within the 100-byte blocks read.
        let mut long = b"%PDF-1.4 ".to_vec();
        long.extend_from_slice(&[b'x'; 200]);
        long.push(b'\n');
        assert_eq!(header_version(&long), Err(err_corrupt_header()));
    }

    #[test]
    fn obj_candidates() {
        assert!(obj_candidate(false, b"1 0 obj"));
        assert!(obj_candidate(false, b"1 0 obj<<>>"));
        assert!(!obj_candidate(true, b"1 0 obj"));
        assert!(!obj_candidate(false, b"endobj"));
        assert!(!obj_candidate(false, b"obj"));
        assert!(!obj_candidate(false, b"1 0 o"));
        let mut within = true;
        let mut line: &[u8] = b"abc endobj 2 0 obj";
        check_end_obj(&mut within, &mut line);
        assert_eq!((within, line), (false, &b"endobj 2 0 obj"[..]));
        assert!(ensure_no_start_xref(b"xref", Some(0)));
        assert!(!ensure_no_start_xref(b"startxref", Some(5)));
        assert!(ensure_no_start_xref(b"1 xref", Some(2)));
        assert!(!ensure_no_start_xref(b"abc", None));
    }

    // Port of TestRegisterXRefObjectsTracksEOLOffsets.
    #[test]
    fn register_xref_objects_tracks_eol_offsets() {
        for eol in ["\n", "\r\n"] {
            let pdf = format!(
                "%PDF-1.7{eol}%\u{80}\u{81}\u{82}\u{83}{eol}7 0 obj{eol}<<>>{eol}endobj{eol}"
            );
            let data = pdf.as_bytes();
            let mut ctx = ctx_for(data, Limits::default());
            let cancel = AtomicBool::new(false);
            let mut rcx = ReadCx::new(data, &cancel, i64::MAX);
            register_xref_objects(&mut rcx, &mut ctx, 1).unwrap();
            let entry = ctx.xref.find(7).unwrap();
            assert_eq!(
                entry.offset,
                Some(pdf.find("7 0 obj").unwrap() as i64),
                "EOL {eol:?}"
            );
            assert_eq!((entry.generation, entry.incr), (Some(0), 1));
        }
    }

    #[test]
    fn repair_entries_are_charged_to_max_alloc() {
        let mut pdf = String::from("%PDF-1.7\n");
        for n in 1..=100 {
            pdf.push_str(&format!("{n} 0 obj\nnull\nendobj\n"));
        }
        let data = pdf.as_bytes();
        let lim = Limits {
            max_alloc: 50 * crate::model::xref::ENTRY_BYTES,
            ..Limits::default()
        };
        let cancel = AtomicBool::new(false);
        for bypass in [false, true] {
            let mut ctx = ctx_for(data, lim);
            let mut rcx = ReadCx::new(data, &cancel, i64::MAX);
            let err = if bypass {
                bypass_xref_section(&mut rcx, &mut ctx, 0, &Error::Corrupt("x".into()), 1)
            } else {
                register_xref_objects(&mut rcx, &mut ctx, 1)
            }
            .unwrap_err();
            assert!(matches!(err, Error::Alloc { .. }), "{err}");
        }
        // The other new-key path, parseAndLoad, charges the entry too.
        let input = b"1 0 obj\ntrue\nendobj\n";
        let mut ctx = ctx_for(input, Limits::default());
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        let mut offset = 0;
        process_object(&mut rcx, &mut ctx, b"1 0 obj", &mut offset, 1).unwrap();
        assert!(ctx.xref.mem().used() >= crate::model::xref::ENTRY_BYTES);
    }

    // Port of TestRepairScannersRespectObjectLimit.
    #[test]
    fn repair_scanners_respect_the_object_limit() {
        for lim in [32u64, 64 << 10] {
            for path in ["register", "repair", "resume"] {
                let input = format!("1 0 obj true endobj\n{}\n", "x".repeat(lim as usize * 2));
                let data = input.as_bytes();
                let mut ctx = ctx_for(data, limit(lim));
                let cancel = AtomicBool::new(false);
                let mut rcx = ReadCx::new(data, &cancel, i64::MAX);
                let err = match path {
                    "register" => register_xref_objects(&mut rcx, &mut ctx, 1).unwrap_err(),
                    "repair" => {
                        bypass_xref_section(&mut rcx, &mut ctx, 0, &Error::Corrupt("x".into()), 1)
                            .unwrap_err()
                    }
                    _ => {
                        let mut offset = 0;
                        let (mut s, _) =
                            process_object(&mut rcx, &mut ctx, b"1 0 obj", &mut offset, 1).unwrap();
                        while s.scan() {}
                        s.err().unwrap()
                    }
                };
                assert!(
                    err.to_string().contains("token too long"),
                    "{path}/limit={lim}: {err}"
                );
            }
        }
    }

    // Port of TestRepairScannerLimitBoundaries.
    #[test]
    fn repair_scanner_limit_boundaries() {
        // (limit, size of the line, fails)
        let cases: [(&str, u64, usize, bool); 4] = [
            ("default", 0, 2 * super::super::DEFAULT_BUF_SIZE, false),
            (
                "grow",
                4 * super::super::DEFAULT_BUF_SIZE as u64,
                2 * super::super::DEFAULT_BUF_SIZE,
                false,
            ),
            ("small fits", 32, 31, false),
            ("small exceeds", 32, 32, true),
        ];
        for (name, lim, size, fail) in cases {
            let input = format!("{}\n", "x".repeat(size));
            let data = input.as_bytes();
            let mut s = new_repair_scanner(&limit(lim), PosReader::at(data, 0).unwrap());
            assert_eq!(!s.scan(), fail, "{name}: {:?}", s.err());
            if fail {
                assert!(
                    s.err()
                        .is_some_and(|e| e.to_string().contains("token too long")),
                    "{name}"
                );
            }
        }
    }

    // Port of TestProcessObjectSkipsBoundedMalformedObjectRelaxed.
    #[test]
    fn process_object_skips_a_bounded_malformed_object() {
        let input = b"1 0 obj\n<</D[0/a0 obj\n<</E 1>>\nendobj\n2 0 obj\nnull\nendobj\n";
        let mut ctx = ctx_for(input, Limits::default());
        let cancel = AtomicBool::new(false);
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        let mut offset = 0;
        let (_, within_obj) =
            process_object(&mut rcx, &mut ctx, b"1 0 obj", &mut offset, 1).unwrap();
        assert!(!within_obj, "scanner remains inside the skipped object");
        let want = find(input, b"endobj").unwrap() as i64 + 6;
        assert_eq!(offset, want);
    }

    // Port of TestProcessObjectResumesWithinValidObject.
    #[test]
    fn process_object_resumes_within_a_valid_object() {
        let input = b"1 0 obj\ntrue\nendobj\n";
        let mut ctx = ctx_for(input, Limits::default());
        let cancel = AtomicBool::new(false);
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        let mut offset = 0;
        let (_, within_obj) =
            process_object(&mut rcx, &mut ctx, b"1 0 obj", &mut offset, 1).unwrap();
        assert!(within_obj);
        assert_eq!(offset, 7);
        assert!(matches!(
            ctx.xref.table[&1].object,
            Some(EntryObject::Obj(Object::Bool(true)))
        ));
        assert_eq!(
            (ctx.xref.table[&1].offset, ctx.xref.table[&1].incr),
            (Some(0), 1)
        );
    }

    // Port of TestPostProcessShiftsOnlyParsedXRefEntries.
    #[test]
    fn post_process_shifts_the_parsed_entries() {
        let mut ctx = ctx_for(b"x", Limits::default());
        ctx.xref.table.insert(1, Entry::free_head());
        ctx.xref.table.insert(3, Entry::gen0(Object::Integer(7)));
        post_process(&mut ctx, 1);
        assert_eq!(ctx.xref.table.len(), 2);
        assert!(ctx.xref.table[&0].free);
        assert!(matches!(
            ctx.xref.table[&2].object,
            Some(EntryObject::Obj(Object::Integer(7)))
        ));

        // Without a free object 1 the head is created from scratch.
        let mut ctx = ctx_for(b"x", Limits::default());
        ctx.xref.table.insert(1, Entry::gen0(Object::Integer(7)));
        post_process(&mut ctx, 1);
        assert!(ctx.xref.table[&0].free && ctx.xref.table.contains_key(&1));
        // More than one section, or an object 0: nothing to do.
        let mut ctx = ctx_for(b"x", Limits::default());
        post_process(&mut ctx, 2);
        assert!(ctx.xref.table.is_empty());
    }

    #[test]
    fn pre_encryption_objects_are_discarded_only_without_a_key() {
        let mut ctx = ctx_for(b"x", Limits::default());
        ctx.xref.encrypt = Some(Ref {
            num: 5,
            generation: 0,
        });
        let mut loaded = Entry::gen0(Object::Integer(1));
        loaded.offset = Some(10);
        ctx.xref.table.insert(1, loaded.clone());
        ctx.xref.table.insert(2, Entry::gen0(Object::Integer(2)));
        ctx.xref.root_dict = Some(Dict::default());
        ctx.xref.enc_key = vec![1];
        discard_pre_encryption_objects(&mut ctx);
        assert!(ctx.xref.table[&1].object.is_some() && ctx.xref.root_dict.is_some());
        ctx.xref.enc_key = Vec::new();
        discard_pre_encryption_objects(&mut ctx);
        assert!(ctx.xref.table[&1].object.is_none());
        // No offset: nothing to read it from again.
        assert!(ctx.xref.table[&2].object.is_some());
        assert!(ctx.xref.root_dict.is_none());
    }

    #[test]
    fn a_decode_limit_error_is_not_repaired() {
        let input = b"1 0 obj true endobj\n";
        let mut ctx = ctx_for(input, Limits::default());
        let cancel = AtomicBool::new(false);
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        let err = Error::DecodeLimit;
        assert_eq!(
            bypass_xref_section(&mut rcx, &mut ctx, 0, &err, 1),
            Err(Error::DecodeLimit)
        );
        assert!(ctx.xref.table.is_empty());
    }

    #[test]
    fn bypass_rebuilds_the_table_from_the_objects() {
        let input = b"%PDF-1.4\n1 0 obj\n<</Type/Catalog>>\nendobj\n2 0 obj\n(x)\nendobj\n";
        let mut ctx = ctx_for(input, Limits::default());
        let cancel = AtomicBool::new(false);
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        bypass_xref_section(&mut rcx, &mut ctx, 0, &Error::Corrupt("x".into()), 1).unwrap();
        let x = &ctx.xref;
        assert!(x.table[&0].free);
        assert_eq!(
            x.root,
            Some(Ref {
                num: 1,
                generation: 0
            })
        );
        // The repaired catalog is the table entry (Go's alias), so no copy is kept.
        assert!(x.root_dict.is_none());
        assert!(matches!(
            x.table[&1].object,
            Some(EntryObject::Obj(Object::Dict(_)))
        ));
        assert!(
            matches!(&x.table[&2].object, Some(EntryObject::Obj(Object::String(s))) if s == b"x")
        );
        assert_eq!(x.table[&2].offset, Some(42));
        // A change through the catalog shows in object 1 (pdf20/i277.d2: validation drops an
        // empty `Outlines`).
        ctx.xref
            .catalog_mut()
            .unwrap()
            .set(b"Outlines", Object::Integer(2));
        assert!(
            matches!(&ctx.xref.table[&1].object, Some(EntryObject::Obj(Object::Dict(d))) if d.has_entry(b"Outlines"))
        );
    }

    #[test]
    fn bypass_keeps_a_catalog_whose_object_was_replaced() {
        let input = b"%PDF-1.4\n1 0 obj\n<</Type/Catalog>>\nendobj\n1 0 obj\n(y)\nendobj\n";
        let mut ctx = ctx_for(input, Limits::default());
        let cancel = AtomicBool::new(false);
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        bypass_xref_section(&mut rcx, &mut ctx, 0, &Error::Corrupt("x".into()), 1).unwrap();
        assert!(ctx.xref.root_dict.is_some());
        assert!(matches!(
            ctx.xref.table[&1].object,
            Some(EntryObject::Obj(Object::String(_)))
        ));
    }

    // Port of TestRepairKeepsObjectsAfterMalformedSkip (the cases without a stream; the stream
    // case needs the stream load of WP 8).
    #[test]
    fn repair_keeps_objects_after_a_malformed_skip() {
        let bad = "1 0 obj\n<</A ]>>\nendobj\n";
        for (name, objects) in [
            ("catalog", bad.to_string()),
            (
                "consecutive malformed",
                format!("{bad}5 0 obj\n<</B ]>>\nendobj\n"),
            ),
        ] {
            let input = format!(
                "%PDF-1.4\n{objects}2 0 obj\n<</Type/Catalog/Pages 3 0 R>>\nendobj\n3 0 obj\n<</Type/Pages/Kids[]/Count 0>>\nendobj\n"
            );
            let data = input.as_bytes();
            let mut ctx = ctx_for(data, Limits::default());
            let cancel = AtomicBool::new(false);
            let mut rcx = ReadCx::new(data, &cancel, i64::MAX);
            read_xref_table(&mut rcx, &mut ctx).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                ctx.xref.root,
                Some(Ref {
                    num: 2,
                    generation: 0
                }),
                "{name}"
            );
            for obj_nr in [2, 3] {
                let loaded = ctx
                    .xref
                    .table
                    .get(&obj_nr)
                    .is_some_and(|e| e.object.is_some());
                assert!(loaded, "{name}: obj#{obj_nr} was lost");
            }
            assert_eq!(ctx.xref.header_version, Some(Version::V14));
        }
    }

    #[test]
    fn a_classic_file_is_read_through_its_xref_section() {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let o1 = pdf.len();
        pdf.extend_from_slice(b"1 0 obj\n<</Type/Catalog>>\nendobj\n");
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!(
                "xref\n0 2\n0000000000 65535 f \n{o1:010} 00000 n \ntrailer\n<</Size 2/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        let mut ctx = ctx_for(&pdf, Limits::default());
        let cancel = AtomicBool::new(false);
        let mut rcx = ReadCx::new(&pdf, &cancel, i64::MAX);
        read_xref_table(&mut rcx, &mut ctx).unwrap();
        let x = &ctx.xref;
        assert_eq!(
            (x.size, x.root),
            (
                Some(2),
                Some(Ref {
                    num: 1,
                    generation: 0
                })
            )
        );
        assert_eq!(x.table[&1].offset, Some(o1 as i64));
        assert!(x.table[&0].free);
        assert_eq!(x.header_version, Some(Version::V14));
        assert_eq!(ctx.read.as_ref().map(|r| r.eol_count), Some(1));
        assert_eq!(ctx.read.as_ref().map(|r| r.using_xref_streams), Some(false));
    }

    #[test]
    fn a_header_offset_is_added_to_every_entry() {
        let mut pdf = b"junk\n".to_vec();
        let base = pdf.len();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        let o1 = pdf.len() - base;
        pdf.extend_from_slice(b"1 0 obj\n<</Type/Catalog>>\nendobj\n");
        let xref = pdf.len() - base;
        pdf.extend_from_slice(
            format!(
                "xref\n0 2\n0000000000 65535 f \n{o1:010} 00000 n \ntrailer\n<</Size 2/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n"
            )
            .as_bytes(),
        );
        let mut ctx = ctx_for(&pdf, Limits::default());
        let cancel = AtomicBool::new(false);
        let mut rcx = ReadCx::new(&pdf, &cancel, i64::MAX);
        read_xref_table(&mut rcx, &mut ctx).unwrap();
        assert_eq!(ctx.xref.table[&1].offset, Some((base + o1) as i64));
    }

    #[test]
    fn a_cancelled_read_stops_the_table_build() {
        let input = b"%PDF-1.4\n1 0 obj true endobj\n";
        let mut ctx = ctx_for(input, Limits::default());
        let cancel = AtomicBool::new(true);
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        assert_eq!(read_xref_table(&mut rcx, &mut ctx), Err(Error::Cancelled));
    }
}
