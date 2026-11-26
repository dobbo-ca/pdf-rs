//! Classic cross-reference sections and the trailer (Go: pkg/pdfcpu/read.go:267-546 and
//! :1091-1625): the last `startxref`, subsections and their entries, the trailer dictionary and
//! its `Prev` and `XRefStm` links.

use super::repair::bypass_xref_section;
use super::xrefstm::parse_hybrid_xref_stream;
use super::{
    ChargedBuf, ReadCx, atoi, bootstrap_integer_entry, contains, err_corrupt_trailer_dict,
    err_missing_scanner_line, find, go_fields, go_trim_space, lim,
    parse_object_with_validation_policy, rfind, to_i64, trim_left, trim_right,
};
use crate::config::Limits;
use crate::error::{Error, Sentinel};
use crate::ledger::Mem;
use crate::model::{Context, Entry, XRefTable};
use crate::object::{Array, Dict, Object, Ref};
use crate::parse::scan::Scanner;
use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------------------------
// The last startxref

/// `incrEpilogIndex`: where a partial `%%EOF` starts at the end of `s`, -1 for none.
fn incr_epilog_index(s: &[u8]) -> Option<usize> {
    [&b"%%EO"[..], b"%%E", b"%%", b"%"]
        .iter()
        .find(|suf| s.ends_with(suf))
        .map(|suf| s.len() - suf.len())
}

/// `offsetLastXRefSection`: the file offset of the last cross-reference section. Goes to the end
/// of the input (less `skip` bytes) and searches backwards, in blocks of 512 bytes, for the
/// first `startxref {offset} %%EOF`.
///
/// # Errors
/// [`Sentinel::MissingXRefSection`] when no block can be read or no `startxref` is found,
/// [`Sentinel::MissingXRefEof`] when no `%%EOF` follows it, an error for an offset that is not
/// a number, and `io.EOF` ([`Sentinel::Eof`]) when a block lies past the end.
pub fn offset_last_xref_section(data: &[u8], skip: i64) -> Result<i64, Error> {
    const MATCH: &[u8] = b"startxref";
    let file_size = to_i64(data.len());
    let mut buf_size: i64 = 512;
    if file_size < buf_size {
        buf_size = file_size;
    }
    if buf_size == 0 {
        return Err(Error::Sentinel(Sentinel::MissingXRefSection));
    }
    let block = usize::try_from(buf_size).map_err(|_| Error::OutOfMemory)?;

    let mut prev_buf: Option<Vec<u8>> = None;
    let mut offset: i64 = 0;
    let mut i: i64 = 1;

    while offset == 0 {
        // Seek(-i*bufSize-skip, io.SeekEnd)
        let pos = i128::from(file_size) - i128::from(i) * i128::from(buf_size) - i128::from(skip);
        if pos < 0 {
            return Err(Error::Sentinel(Sentinel::MissingXRefSection));
        }
        let pos = usize::try_from(pos).unwrap_or(usize::MAX);

        // curBuf := make([]byte, bufSize); fillBuffer(rs, curBuf)
        let avail = data.get(pos..).unwrap_or(&[]);
        let n = avail.len().min(block);
        if n == 0 {
            return Err(Error::Sentinel(Sentinel::Eof).context("scan for startxref"));
        }
        let mut cur_buf: Vec<u8> = Vec::new();
        cur_buf.try_reserve_exact(block)?;
        cur_buf.extend_from_slice(&avail[..n]);
        cur_buf.resize(block, 0);

        let mut work_buf: Vec<u8> = Vec::new();
        work_buf.try_reserve_exact(block + prev_buf.as_ref().map_or(0, Vec::len))?;
        work_buf.extend_from_slice(&cur_buf);
        if let Some(p) = &prev_buf {
            work_buf.extend_from_slice(p);
        }

        let Some(j) = rfind(&work_buf, MATCH) else {
            prev_buf = Some(cur_buf);
            i += 1;
            continue;
        };

        let start = j + MATCH.len();
        if start >= work_buf.len() {
            return Err(Error::Sentinel(Sentinel::MissingXRefEof));
        }

        let p = &work_buf[start + 1..];
        let pos_eof = match find(p, b"%%EOF").or_else(|| incr_epilog_index(p)) {
            Some(n) => n,
            None => return Err(Error::Sentinel(Sentinel::MissingXRefEof)),
        };

        let p = &p[..pos_eof];
        let s = go_trim_space(p);
        let s = go_trim_space(trim_right(s, b"%"));
        offset = atoi(s).map_err(|_| Error::Corrupt("invalid last xref section".to_string()))?;
        if offset >= file_size {
            offset = 0;
        }
        i += 1;
    }

    Ok(offset)
}

// ---------------------------------------------------------------------------------------------
// Entries and subsections

/// `createXRefTableEntry`: the entry for one line of a classic xref subsection, and whether the
/// line makes one.
fn create_xref_table_entry(
    in_use: bool,
    obj_nr: i64,
    offset: i64,
    off_extra: i64,
    generation: i64,
    incr: i64,
) -> (Entry, bool) {
    let mut entry = Entry {
        offset: Some(offset),
        generation: Some(generation),
        incr,
        ..Entry::default()
    };

    if in_use {
        // in use object
        if offset == 0 {
            if obj_nr == 0 {
                entry.free = true;
                return (entry, true);
            }
            return (entry, false);
        }

        if offset < 9 {
            return (entry, false);
        }

        entry.offset = Some(offset.wrapping_add(off_extra));

        return (entry, true);
    }

    // free object
    entry.free = true;

    (entry, true)
}

/// `decodeSubsection`: the offset, generation and type (`b'f'` or `b'n'`) of one entry line.
///
/// # Errors
/// A number that does not parse, and an entry type other than `f` and `n`
/// ([`Sentinel::CorruptXRefSubsection`]).
pub fn decode_subsection(fields: &[&[u8]]) -> Result<(i64, i64, u8), Error> {
    let [f0, f1, f2] = fields else {
        return Err(Error::Panic("xref entry needs three fields"));
    };
    let mut s: &[u8] = f0;
    while let Some((&c, rest)) = s.split_first()
        && !c.is_ascii_digit()
    {
        s = rest;
    }

    let offset = atoi(s)?;

    let generation: &[u8] = if f1.len() <= 5 { f1 } else { b"00000" };
    let generation = atoi(generation)?;

    let entry_type = match *f2 {
        b"f" => b'f',
        b"n" => b'n',
        _ => return Err(Error::Sentinel(Sentinel::CorruptXRefSubsection)),
    };

    Ok((offset, generation, entry_type))
}

/// `parseXRefTableEntry`: reads the next subsection entry and generates the corresponding xref
/// table entry. An in-use entry of a numbered object with offset 0 is skipped.
///
/// # Errors
/// The errors of [`decode_subsection`]; [`Error::Alloc`] for the entry.
pub fn parse_xref_table_entry(
    x: &mut XRefTable,
    fields: &[&[u8]],
    obj_nr: i64,
    off_extra: i64,
    incr: i64,
) -> Result<(), Error> {
    let (offset, generation, entry_type) = decode_subsection(fields)?;

    let in_use = entry_type == b'n';
    if in_use && obj_nr > 0 && offset == 0 {
        // Relaxed: the entry is skipped.
        return Ok(());
    }

    let (entry, ok) = create_xref_table_entry(in_use, obj_nr, offset, off_extra, generation, incr);
    if !ok {
        return Ok(());
    }

    x.insert_charged(obj_nr, entry)
}

/// `parseXRefTableSubsectionHeader`: the first object number and the object count of a
/// subsection.
///
/// # Errors
/// Numbers that do not parse, negative ones ([`Sentinel::CorruptXRefSubsection`]), a range over
/// `max_object_count` ([`Error::XRefObjectRange`]), more entries than `max_xref_entries`.
pub fn parse_xref_table_subsection_header(
    fields: &[&[u8]],
    entry_count: i64,
    limits: &Limits,
) -> Result<(i64, i64), Error> {
    let [f0, f1] = fields else {
        return Err(Error::Panic("xref subsection header needs two fields"));
    };
    let start_obj_number = atoi(f0)?;
    let obj_count = atoi(f1)?;

    if start_obj_number < 0 || obj_count < 0 {
        return Err(Error::Sentinel(Sentinel::CorruptXRefSubsection));
    }
    let max_objects = lim(limits.max_object_count);
    if obj_count > max_objects || start_obj_number > max_objects - obj_count {
        return Err(Error::XRefObjectRange);
    }
    let max_entries = lim(limits.max_xref_entries);
    if entry_count > max_entries || obj_count > max_entries - entry_count {
        return Err(Error::Corrupt(format!(
            "xref entry count exceeds limit {max_entries}"
        )));
    }

    Ok((start_obj_number, obj_count))
}

/// `xRefTableObjectNumber`: the object number of entry `index` of a subsection.
fn xref_table_object_number(
    start_obj_number: i64,
    index: i64,
    entry_count: i64,
    limits: &Limits,
) -> Result<i64, Error> {
    let max_entries = lim(limits.max_xref_entries);
    if entry_count >= max_entries {
        return Err(Error::Corrupt(format!(
            "xref entry count exceeds limit {max_entries}"
        )));
    }
    if index >= lim(limits.max_object_count) - start_obj_number {
        return Err(Error::XRefObjectRange);
    }
    Ok(start_obj_number + index)
}

/// `parseXRefTableSubSection`: processes the subsections of an xref section into xref table
/// entries and returns the line that ends them (the trailer line).
///
/// # Errors
/// The errors of the header and entry parsers, the scanner, and an incomplete subsection;
/// [`Error::Cancelled`].
pub fn parse_xref_table_subsection<'a>(
    cancel: &AtomicBool,
    x: &mut XRefTable,
    limits: &Limits,
    s: &mut Scanner<'a>,
    mut fields: Vec<&'a [u8]>,
    off_extra: i64,
    incr: i64,
) -> Result<&'a [u8], Error> {
    let mut line: &'a [u8] = &[];
    let mut trailer = false;
    let mut entry_count: i64 = 0;

    while !trailer && fields.len() == 2 {
        let (start_obj_number, obj_count) =
            parse_xref_table_subsection_header(&fields, entry_count, limits)?;

        // Process all entries of this subsection into xRefTable entries.
        let mut i: i64 = 0;
        loop {
            line = scan_line(s)?;

            fields = go_fields(line)?;
            if fields.len() != 3 {
                if i < obj_count {
                    return Err(Error::Corrupt("incomplete xref subsection".to_string()));
                }
                trailer = contains(line, b"trailer");
                break;
            }

            let obj_nr = xref_table_object_number(start_obj_number, i, entry_count, limits)?;
            if entry_count % 4096 == 0 && cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            entry_count += 1;

            // Skip an entry that is already assigned (a newer section has it), and a line
            // whose offset is a single digit (it should be 10).
            if !x.exists(obj_nr) && fields[0].len() != 1 {
                parse_xref_table_entry(x, &fields, obj_nr, off_extra, incr)?;
            }
            i += 1;
        }
    }

    Ok(line)
}

// ---------------------------------------------------------------------------------------------
// The trailer

/// `directTrailerIntegerEntry`: the entry `key` of the trailer, which must be a direct integer.
fn direct_trailer_integer_entry(d: &Dict, key: &str) -> Result<Option<i64>, Error> {
    match d.find(key.as_bytes()) {
        None | Some(Object::Null) => Ok(None),
        Some(Object::Integer(i)) => Ok(Some(*i)),
        Some(_) => Err(err_corrupt_trailer_dict()
            .context(format!("trailer entry {key} must be a direct integer"))),
    }
}

/// `parseTrailerSize`. An invalid `Size` is not an error in relaxed mode: it is left unset, so
/// that an older trailer or the completed object walk can supply it.
fn parse_trailer_size(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let Some(v) = direct_trailer_integer_entry(d, "Size")? else {
        return Err(Error::Corrupt("missing trailer Size".to_string()));
    };
    // Not reliable! Patched after all read in.
    let limit = lim(x.conf.limits.max_object_count);
    if v <= 0 || v > limit {
        return Ok(());
    }
    x.size = Some(v);
    Ok(())
}

/// `parseTrailerRoot`.
fn parse_trailer_root(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let Some(ind_ref) = d.indirect_ref_entry(b"Root") else {
        return Err(Error::Corrupt("missing trailer Root".to_string()));
    };
    x.root = Some(ind_ref);
    Ok(())
}

/// `parseTrailerInfo`. A direct info dictionary waits for the PDF version.
fn parse_trailer_info(x: &mut XRefTable, d: &Dict) {
    if let Some(ind_ref) = d.indirect_ref_entry(b"Info") {
        x.info = Some(ind_ref);
        return;
    }
    if let Some(info_dict) = d.dict_entry(b"Info") {
        x.direct_info_dict = Some(info_dict.clone());
    }
}

/// `materializeDirectInfoDict`: a direct trailer info dictionary becomes an indirect object in
/// a PDF 2.0 file, and is dropped from older ones.
///
/// # Errors
/// [`Error::Panic`] where Go dereferences a nil header version.
pub fn materialize_direct_info_dict(x: &mut XRefTable) -> Result<(), Error> {
    if x.direct_info_dict.is_none() {
        return Ok(());
    }
    if !x.pdf20()? {
        x.direct_info_dict = None;
        return Ok(());
    }
    let Some(info_dict) = x.direct_info_dict.take() else {
        return Ok(());
    };
    let size = match x.size {
        Some(s) => s,
        // All existing objects have been read; allocate beyond their highest object number.
        None => x.max_obj_nr.saturating_add(1),
    };

    let mut obj_nr = size;
    while x.table.contains_key(&obj_nr) {
        obj_nr = obj_nr.saturating_add(1);
    }
    let mut entry = Entry::gen0(Object::Dict(info_dict));
    entry.ref_count = 1;
    x.table.insert(obj_nr, entry);
    x.size = Some(obj_nr.saturating_add(1));
    if obj_nr > x.max_obj_nr {
        x.max_obj_nr = obj_nr;
    }
    x.info = Some(Ref {
        num: obj_nr,
        generation: 0,
    });
    Ok(())
}

/// `parseTrailerID`. A one-element array is repaired by repeating its element.
fn parse_trailer_id(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    if let Some(arr) = d.array_entry(b"ID") {
        let mut arr: Array = arr.clone();
        if arr.len() != 2 {
            if arr.len() != 1 {
                return Err(Error::Corrupt("invalid trailer ID".to_string()));
            }
            arr.push(arr[0].clone());
        }
        x.id = Some(arr);
    }
    // A missing ID of an encrypted file is only an error in strict mode.
    Ok(())
}

/// `parseTrailer`: takes `Encrypt`, `Size`, `Root`, `Info` and `ID` from a trailer dictionary,
/// the first one that gives each of them (the newest section is read first).
///
/// # Errors
/// A missing `Size` or `Root`, a `Size` that is not a direct integer, an invalid `ID`.
pub fn parse_trailer(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    if let Some(ind_ref) = d.indirect_ref_entry(b"Encrypt") {
        x.encrypt = Some(ind_ref);
    }

    if x.size.is_none() {
        parse_trailer_size(x, d)?;
    }

    if x.root.is_none() {
        parse_trailer_root(x, d)?;
    }

    if x.info.is_none() && x.direct_info_dict.is_none() {
        parse_trailer_info(x, d);
    }

    if x.id.is_none() {
        parse_trailer_id(x, d)?;
    }

    Ok(())
}

/// `scanForPreviousXref`: where the `xref` keyword before `offset` is (the `startxref` of the
/// section is looked for first, then the keyword before it).
pub fn scan_for_previous_xref(data: &[u8], offset: i64) -> Option<i64> {
    const MATCH1: &[u8] = b"startxref";
    const MATCH2: &[u8] = b"xref";
    const BUF_SIZE: i64 = 512;
    let block = BUF_SIZE as usize;

    let mut prev_buf: Option<Vec<u8>> = None;
    let mut m = MATCH1;
    let mut i: i64 = 1;

    loop {
        let off = offset.checked_sub(i.checked_mul(BUF_SIZE)?)?;
        // newPositionedReader: a negative position is an error.
        let pos = usize::try_from(off).ok()?;

        // fillBuffer: nothing to read is io.EOF, which ends the scan.
        let avail = data.get(pos..).unwrap_or(&[]);
        let n = avail.len().min(block);
        if n == 0 {
            return None;
        }
        let mut cur_buf: Vec<u8> = Vec::new();
        cur_buf.try_reserve_exact(block).ok()?;
        cur_buf.extend_from_slice(&avail[..n]);
        cur_buf.resize(block, 0);

        let mut work_buf: Vec<u8> = Vec::new();
        work_buf
            .try_reserve_exact(block + prev_buf.as_ref().map_or(0, Vec::len))
            .ok()?;
        work_buf.extend_from_slice(&cur_buf);
        if let Some(p) = &prev_buf {
            work_buf.extend_from_slice(p);
        }

        let Some(j) = rfind(&work_buf, m) else {
            if n < block {
                return None;
            }
            prev_buf = Some(cur_buf);
            i += 1;
            continue;
        };

        if m == MATCH1 {
            m = MATCH2;
            i += 1;
            continue;
        }

        return off.checked_add(to_i64(j));
    }
}

/// `handleAdditionalStreams`: the references of the trailer's `AdditionalStreams`.
fn handle_additional_streams(trailer_dict: &Dict, x: &mut XRefTable) {
    let Some(arr) = trailer_dict.array_entry(b"AdditionalStreams") else {
        return;
    };
    x.additional_streams = Some(
        arr.iter()
            .filter(|o| matches!(o, Object::Ref(_)))
            .cloned()
            .collect(),
    );
}

/// `offsetPrev`: the offset of the previous cross-reference section, if the trailer has one. A
/// `Prev` of 0 is looked up by scanning back from the current section.
///
/// # Errors
/// A `Prev` that is not a direct integer.
fn offset_prev(
    data: &[u8],
    trailer_dict: &Dict,
    off_cur_xref: Option<i64>,
) -> Result<Option<i64>, Error> {
    let Some(v) = direct_trailer_integer_entry(trailer_dict, "Prev")? else {
        return Ok(None);
    };
    if v == 0 {
        return Ok(off_cur_xref.and_then(|cur| scan_for_previous_xref(data, cur)));
    }
    Ok(Some(v))
}

/// `xrefStreamOffset`: the `XRefStm` entry of a hybrid file's trailer. Not read while repairing.
fn xref_stream_offset(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    trailer_dict: &Dict,
    repairing: bool,
) -> Result<Option<i64>, Error> {
    if repairing {
        return Ok(None);
    }
    let (i, _) = bootstrap_integer_entry(rcx, ctx, trailer_dict, b"XRefStm")
        .map_err(|e| e.context("parse trailer XRefStm"))?;
    Ok(i)
}

/// `parseTrailerDict`: processes a trailer dictionary and returns the offset of the previous
/// xref section. A hybrid file's `XRefStm` is read first; if that fails, the table is rebuilt
/// by repair and there is no previous section.
///
/// # Errors
/// The trailer errors, the `XRefStm` read errors that repair cannot fix.
pub fn parse_trailer_dict(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    trailer_dict: &Dict,
    off_cur_xref: Option<i64>,
    off_extra: i64,
    incr: i64,
    repairing: bool,
) -> Result<Option<i64>, Error> {
    parse_trailer(&mut ctx.xref, trailer_dict)?;

    handle_additional_streams(trailer_dict, &mut ctx.xref);

    let offset = offset_prev(rcx.data, trailer_dict, off_cur_xref)?;

    let Some(mut offset_xref_stream) = xref_stream_offset(rcx, ctx, trailer_dict, repairing)?
    else {
        // No cross reference stream. Continue to parse previous xref section, if there is any.
        return Ok(offset);
    };

    // This file is using cross reference streams.

    if !super::read_info(ctx)?.hybrid {
        let ri = super::read_info(ctx)?;
        ri.hybrid = true;
        ri.using_xref_streams = true;
    }

    // 1.5 conformant readers process hidden objects contained in XRefStm before continuing to
    // process any previous XRefSection. Previous XRefSection is expected to have free entries
    // for hidden entries. May appear in XRefSections only.
    if let Err(err) = parse_hybrid_xref_stream(rcx, ctx, &mut offset_xref_stream, off_extra, incr) {
        bypass_xref_section(rcx, ctx, off_extra, &err, incr)?;
        return Ok(None);
    }

    Ok(offset)
}

// ---------------------------------------------------------------------------------------------
// Lines

/// `scanLineRaw`: the next line of the scanner.
///
/// # Errors
/// The scanner's error, or [`err_missing_scanner_line`] at the end of the input.
pub fn scan_line_raw<'a>(s: &mut Scanner<'a>) -> Result<&'a [u8], Error> {
    if !s.scan() {
        if let Some(e) = s.err() {
            return Err(e);
        }
        return Err(err_missing_scanner_line());
    }
    Ok(s.bytes())
}

/// `scanLine`: the next line of the scanner that is not empty (one empty line is skipped).
///
/// # Errors
/// As [`scan_line_raw`].
pub fn scan_line<'a>(s: &mut Scanner<'a>) -> Result<&'a [u8], Error> {
    let mut line: &'a [u8] = &[];
    for _ in 0..=1 {
        line = scan_line_raw(s)?;
        if !line.is_empty() {
            break;
        }
    }
    Ok(line)
}

/// `scanTrailerDictRemainder` of `scanTrailer`: the text of the trailer, from its `<<` on, up to
/// the `startxref` line, one line per line.
fn scan_trailer(mem: &Mem, s: &mut Scanner<'_>, line: &[u8]) -> Result<ChargedBuf, Error> {
    // scanTrailerDictStart
    let mut text: &[u8] = line;
    loop {
        if let Some(i) = find(text, b"<<") {
            text = &text[i..];
            break;
        }
        text = scan_line(s)?;
    }

    // scanTrailerDictRemainder
    let mut buf = ChargedBuf::new(mem)?;
    let mut i = find(text, b"startxref");
    while i.is_none() {
        buf.extend(text)?;
        buf.push(b'\n')?;
        text = scan_line(s)?;
        i = find(text, b"startxref");
    }
    let i = i.unwrap_or(0);
    buf.extend(&text[..i])?;
    buf.push(b'\n')?;
    Ok(buf)
}

/// `processTrailer`: parses the trailer that starts at `line` (the `trailer` keyword and what
/// follows it on that line) and returns the offset of the previous xref section.
///
/// # Errors
/// The scanner errors, the parse errors, and a trailer that is not a dictionary.
pub fn process_trailer(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    s: &mut Scanner<'_>,
    line: &[u8],
    off_cur_xref: Option<i64>,
    off_extra: i64,
    incr: i64,
    repairing: bool,
) -> Result<Option<i64>, Error> {
    let trailer_string: &[u8] = if line != b"trailer" {
        line.get(7..)
            .ok_or(Error::Panic("trailer line too short"))?
    } else {
        &[]
    };

    let mem = ctx.xref.mem().clone();
    let text = scan_trailer(&mem, s, trailer_string)?;

    let mut rest: &[u8] = &text.v;
    let o = parse_object_with_validation_policy(rcx, ctx, &mut rest)?;

    let Object::Dict(trailer_dict) = o else {
        return Err(err_corrupt_trailer_dict());
    };

    parse_trailer_dict(
        rcx,
        ctx,
        &trailer_dict,
        off_cur_xref,
        off_extra,
        incr,
        repairing,
    )
}

/// `parseXRefSection`: parses an xref section into xref table entries. `fields` are the fields
/// of an entry line that has already been scanned, or empty. Returns the offset of the previous
/// section.
///
/// # Errors
/// The errors of the subsection and trailer parsers, and [`Sentinel::MissingTrailerDict`].
pub fn parse_xref_section<'a>(
    rcx: &mut ReadCx<'_>,
    ctx: &mut Context,
    s: &mut Scanner<'a>,
    fields: &[&'a [u8]],
    ss_count: &mut i64,
    off_cur_xref: i64,
    off_extra: i64,
    incr: i64,
) -> Result<Option<i64>, Error> {
    let mut line: &'a [u8] = &[];
    let mut fields: Vec<&'a [u8]> = fields.to_vec();

    if fields.is_empty() {
        line = scan_line(s)?;
        fields = go_fields(line)?;
    }

    line = trim_left(line, b" ");
    if !line.starts_with(b"trailer") {
        // Process all sub sections of this xRef section.
        let limits = ctx.xref.conf.limits;
        line = parse_xref_table_subsection(
            rcx.cancel,
            &mut ctx.xref,
            &limits,
            s,
            fields,
            off_extra,
            incr,
        )?;
        *ss_count += 1;
        line = trim_left(line, b" ");
    }

    if !line.starts_with(b"trailer") {
        return Err(Error::Sentinel(Sentinel::MissingTrailerDict).context("xrefsection"));
    }

    process_trailer(
        rcx,
        ctx,
        s,
        line,
        Some(off_cur_xref),
        off_extra,
        incr,
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::parse::scan::lines;
    use std::sync::atomic::AtomicBool;

    fn table() -> XRefTable {
        XRefTable::new(Config::default())
    }

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

    // Port of TestOffsetLastXRefSectionClassifiesEpilogueErrors.
    #[test]
    fn offset_last_xref_section_classifies_epilogue_errors() {
        let cases: [(&str, &[u8], Option<Sentinel>); 4] = [
            (
                "missing EOF",
                b"%PDF-1.7\nstartxref\n12\n",
                Some(Sentinel::MissingXRefEof),
            ),
            (
                "truncated after startxref",
                b"%PDF-1.7\nstartxref",
                Some(Sentinel::MissingXRefEof),
            ),
            (
                "invalid offset",
                b"%PDF-1.7\nstartxref\nnot-an-offset\n%%EOF",
                None,
            ),
            (
                "missing startxref",
                b"%PDF-1.7\n%%EOF",
                Some(Sentinel::MissingXRefSection),
            ),
        ];
        for (name, input, want) in cases {
            let err = offset_last_xref_section(input, 0).unwrap_err();
            match want {
                Some(s) => assert!(err.is(s), "{name}: {err}"),
                None => assert!(
                    err.to_string().contains("invalid last xref section"),
                    "{name}: {err}"
                ),
            }
        }
    }

    #[test]
    fn offset_last_xref_section_finds_the_offset() {
        let input = b"%PDF-1.7\nxref\n0 0\ntrailer\n<<>>\nstartxref\n9\n%%EOF\n";
        assert_eq!(offset_last_xref_section(input, 0), Ok(9));
        // The incomplete epilogs of a truncated file.
        for tail in ["%%EO", "%%E", "%%", "%"] {
            let mut v = b"%PDF-1.7\nstartxref\n9\n".to_vec();
            v.extend_from_slice(tail.as_bytes());
            assert_eq!(offset_last_xref_section(&v, 0), Ok(9), "tail {tail:?}");
        }
        let err = offset_last_xref_section(b"%PDF-1.7\nstartxref\n9\n", 0).unwrap_err();
        assert!(err.is(Sentinel::MissingXRefEof));
        // An offset at or after the end of the file is no offset: the scan goes on, and ends.
        let err = offset_last_xref_section(b"%PDF-1.7\nstartxref\n99\n%%EOF", 0).unwrap_err();
        assert!(err.is(Sentinel::MissingXRefSection));
    }

    #[test]
    fn offset_last_xref_section_looks_across_blocks() {
        let mut v = b"%PDF-1.7\n".to_vec();
        v.extend_from_slice(&[b'x'; 2000]);
        v.extend_from_slice(b"\nstartxref\n9\n%%EOF\n");
        v.extend_from_slice(&[b' '; 700]);
        assert_eq!(offset_last_xref_section(&v, 0), Ok(9));
    }

    #[test]
    fn decode_subsection_cases() {
        let ok = decode_subsection(&[b"0000000016", b"00000", b"n"]).unwrap();
        assert_eq!(ok, (16, 0, b'n'));
        // Leading non-digits are dropped from the offset, a long generation is 0.
        let ok = decode_subsection(&[b"-0000000016", b"123456", b"f"]).unwrap();
        assert_eq!(ok, (16, 0, b'f'));
        // Port of TestDecodeSubsectionClassifiesCorruptEntryType.
        let err = decode_subsection(&[b"0000000000", b"65535", b"x"]).unwrap_err();
        assert!(err.is(Sentinel::CorruptXRefSubsection));
        assert!(decode_subsection(&[b"abc", b"0", b"n"]).is_err());
    }

    // Port of TestParseXRefTableSubSectionClassifiesIncompleteSubsection.
    #[test]
    fn incomplete_subsection_is_an_error() {
        let mut x = table();
        let mut s = Scanner::new(b"trailer\n", lines);
        let err = parse_xref_table_subsection(
            &AtomicBool::new(false),
            &mut x,
            &Limits::default(),
            &mut s,
            vec![&b"0"[..], &b"1"[..]],
            0,
            0,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("incomplete xref subsection"),
            "{err}"
        );
    }

    #[test]
    fn subsection_entries_stop_on_cancel() {
        let mut x = table();
        let mut s = Scanner::new(b"0000000009 00000 n \ntrailer\n", lines);
        let err = parse_xref_table_subsection(
            &AtomicBool::new(true),
            &mut x,
            &Limits::default(),
            &mut s,
            vec![&b"1"[..], &b"1"[..]],
            0,
            0,
        )
        .unwrap_err();
        assert_eq!(err, Error::Cancelled);
    }

    #[test]
    fn subsection_entries_become_table_entries() {
        let mut x = table();
        let text = b"0000000000 65535 f \n0000000009 00000 n \n0000000000 00000 n \n3 1\n0000000100 00001 n \ntrailer\n<<>>\n";
        let mut s = Scanner::new(text, lines);
        let line = parse_xref_table_subsection(
            &AtomicBool::new(false),
            &mut x,
            &Limits::default(),
            &mut s,
            vec![&b"0"[..], &b"3"[..]],
            5,
            2,
        )
        .unwrap();
        assert_eq!(line, b"trailer");
        // Object 0 is free, 1 is in use (offset + 5), 2 has offset 0 and is skipped.
        assert!(x.table[&0].free);
        assert_eq!(x.table[&1].offset, Some(14));
        assert_eq!((x.table[&1].generation, x.table[&1].incr), (Some(0), 2));
        assert!(!x.exists(2));
        assert_eq!(x.table[&3].offset, Some(105));
        assert_eq!(x.table[&3].generation, Some(1));
    }

    #[test]
    fn entries_that_are_already_assigned_win() {
        let mut x = table();
        x.table.insert(1, Entry::default());
        let mut s = Scanner::new(b"0000000999 00000 n \ntrailer\n", lines);
        parse_xref_table_subsection(
            &AtomicBool::new(false),
            &mut x,
            &Limits::default(),
            &mut s,
            vec![&b"1"[..], &b"1"[..]],
            0,
            1,
        )
        .unwrap();
        assert_eq!(x.table[&1].offset, None);
    }

    #[test]
    fn subsection_header_limits() {
        let l = Limits {
            max_object_count: 10,
            max_xref_entries: 5,
            ..Limits::default()
        };
        assert_eq!(
            parse_xref_table_subsection_header(&[b"2", b"3"], 0, &l),
            Ok((2, 3))
        );
        let err = parse_xref_table_subsection_header(&[b"-1", b"3"], 0, &l).unwrap_err();
        assert!(err.is(Sentinel::CorruptXRefSubsection));
        let err = parse_xref_table_subsection_header(&[b"9", b"3"], 0, &l).unwrap_err();
        assert_eq!(err, Error::XRefObjectRange);
        assert!(err.is(Sentinel::CorruptXRefSubsection));
        let err = parse_xref_table_subsection_header(&[b"0", b"3"], 3, &l).unwrap_err();
        assert!(err.to_string().contains("xref entry count exceeds limit 5"));
        assert_eq!(xref_table_object_number(2, 3, 0, &l), Ok(5));
        assert_eq!(
            xref_table_object_number(2, 8, 0, &l),
            Err(Error::XRefObjectRange)
        );
        assert!(xref_table_object_number(2, 1, 5, &l).is_err());
    }

    // Port of TestParseXRefTableEntryZeroOffsetPolicy (relaxed half).
    #[test]
    fn in_use_entry_with_offset_zero_is_skipped() {
        let mut x = table();
        parse_xref_table_entry(&mut x, &[b"0000000000", b"00000", b"n"], 3, 0, 1).unwrap();
        assert!(!x.exists(3));
        // Object 0 with offset 0 is the free head.
        parse_xref_table_entry(&mut x, &[b"0000000000", b"65535", b"n"], 0, 0, 1).unwrap();
        assert!(x.table[&0].free);
        // An offset below 9 is no object.
        parse_xref_table_entry(&mut x, &[b"0000000005", b"00000", b"n"], 4, 0, 1).unwrap();
        assert!(!x.exists(4));
    }

    // Port of TestScanLineRawClassifiesMissingLine.
    #[test]
    fn scan_line_raw_classifies_a_missing_line() {
        let mut s = Scanner::new(b"", lines);
        let err = scan_line_raw(&mut s).unwrap_err();
        assert!(super::super::is_missing_scanner_line(&err));
        let mut s = Scanner::new(b"\n\n\nabc\n", lines);
        // One empty line is skipped, a second one is returned.
        assert_eq!(scan_line(&mut s).unwrap(), b"");
        assert_eq!(scan_line(&mut s).unwrap(), b"abc");
    }

    // Port of TestParseTrailerClassifiesMissingEntries.
    #[test]
    fn parse_trailer_classifies_missing_entries() {
        let mut x = table();
        let err = parse_trailer(&mut x, &Dict::default()).unwrap_err();
        assert!(err.to_string().contains("missing trailer Size"), "{err}");

        let mut x = table();
        let d = dict(&[(
            "Size",
            Object::Ref(Ref {
                num: 7,
                generation: 0,
            }),
        )]);
        let err = parse_trailer(&mut x, &d).unwrap_err();
        assert_eq!(err.class(), "syntax");
        assert!(
            err.to_string().contains("must be a direct integer"),
            "{err}"
        );

        let mut x = table();
        let d = dict(&[("Size", Object::Integer(7))]);
        let err = parse_trailer(&mut x, &d).unwrap_err();
        assert!(err.to_string().contains("missing trailer Root"), "{err}");

        let mut x = table();
        let d = dict(&[
            ("Size", Object::Integer(7)),
            (
                "Root",
                Object::Ref(Ref {
                    num: 1,
                    generation: 0,
                }),
            ),
            (
                "ID",
                Object::Array(vec![
                    Object::Hex(b"AB".to_vec()),
                    Object::Hex(b"CD".to_vec()),
                    Object::Hex(b"EF".to_vec()),
                ]),
            ),
        ]);
        let err = parse_trailer(&mut x, &d).unwrap_err();
        assert!(err.to_string().contains("invalid trailer ID"), "{err}");
    }

    #[test]
    fn parse_trailer_takes_the_first_value_of_each_entry() {
        let mut x = table();
        let newest = dict(&[
            ("Size", Object::Integer(7)),
            (
                "Root",
                Object::Ref(Ref {
                    num: 1,
                    generation: 0,
                }),
            ),
            (
                "Info",
                Object::Ref(Ref {
                    num: 2,
                    generation: 0,
                }),
            ),
            (
                "Encrypt",
                Object::Ref(Ref {
                    num: 3,
                    generation: 0,
                }),
            ),
            ("ID", Object::Array(vec![Object::Hex(b"AB".to_vec())])),
        ]);
        parse_trailer(&mut x, &newest).unwrap();
        let older = dict(&[
            ("Size", Object::Integer(3)),
            (
                "Root",
                Object::Ref(Ref {
                    num: 9,
                    generation: 0,
                }),
            ),
        ]);
        parse_trailer(&mut x, &older).unwrap();
        assert_eq!(x.size, Some(7));
        assert_eq!(
            x.root,
            Some(Ref {
                num: 1,
                generation: 0
            })
        );
        assert_eq!(
            x.info,
            Some(Ref {
                num: 2,
                generation: 0
            })
        );
        assert_eq!(
            x.encrypt,
            Some(Ref {
                num: 3,
                generation: 0
            })
        );
        // A one-element ID is repeated.
        assert_eq!(x.id.as_ref().map(Vec::len), Some(2));
    }

    // Port of TestParseTrailerSizeDiscardsInvalidValue.
    #[test]
    fn invalid_trailer_size_is_left_unset() {
        for size in [0, -3, 10_000_001] {
            let mut x = table();
            let d = dict(&[
                ("Size", Object::Integer(size)),
                (
                    "Root",
                    Object::Ref(Ref {
                        num: 1,
                        generation: 0,
                    }),
                ),
            ]);
            parse_trailer(&mut x, &d).unwrap();
            assert_eq!(x.size, None, "size {size}");
            // An older trailer can supply it.
            let older = dict(&[("Size", Object::Integer(4))]);
            parse_trailer(&mut x, &older).unwrap();
            assert_eq!(x.size, Some(4));
        }
    }

    // Port of TestOffsetPrevRejectsIndirectValue.
    #[test]
    fn offset_prev_cases() {
        let d = dict(&[(
            "Prev",
            Object::Ref(Ref {
                num: 7,
                generation: 0,
            }),
        )]);
        assert!(offset_prev(b"", &d, None).is_err());
        assert_eq!(offset_prev(b"", &Dict::default(), None), Ok(None));
        let d = dict(&[("Prev", Object::Integer(42))]);
        assert_eq!(offset_prev(b"", &d, None), Ok(Some(42)));
        // A Prev of 0 with nothing to scan.
        let d = dict(&[("Prev", Object::Integer(0))]);
        assert_eq!(offset_prev(b"", &d, None), Ok(None));
        assert_eq!(offset_prev(b"%PDF-1.7\n", &d, Some(9)), Ok(None));
    }

    #[test]
    fn scan_for_previous_xref_finds_the_section_before() {
        // The `startxref` is searched first (block 2), then the `xref` before it (block 3).
        let mut v = vec![b' '; 1700];
        v[200..204].copy_from_slice(b"xref");
        v[800..809].copy_from_slice(b"startxref");
        assert_eq!(scan_for_previous_xref(&v, 1700), Some(200));
        // A position before the first block is no position.
        assert_eq!(scan_for_previous_xref(&v, 2), None);
        assert_eq!(scan_for_previous_xref(b"short", 3), None);
        // A file that has no `startxref` before the offset ends the scan at its start.
        assert_eq!(scan_for_previous_xref(&vec![b' '; 1700], 1700), None);
    }

    #[test]
    fn additional_streams_keep_only_references() {
        let mut x = table();
        let d = dict(&[(
            "AdditionalStreams",
            Object::Array(vec![
                Object::Ref(Ref {
                    num: 4,
                    generation: 0,
                }),
                Object::Integer(3),
                Object::Ref(Ref {
                    num: 5,
                    generation: 1,
                }),
            ]),
        )]);
        handle_additional_streams(&d, &mut x);
        assert_eq!(x.additional_streams.as_ref().map(Vec::len), Some(2));
        handle_additional_streams(&Dict::default(), &mut x);
        assert_eq!(x.additional_streams.as_ref().map(Vec::len), Some(2));
    }

    // Port of TestParseTrailerInfoMaterializesDirectPDF20Dictionary and
    // TestParseTrailerInfoLeavesDirectPDF17DictionaryUnsupported.
    #[test]
    fn direct_info_dict_is_materialized_in_pdf20_only() {
        for (version, kept) in [
            (crate::model::Version::V20, true),
            (crate::model::Version::V17, false),
        ] {
            let mut x = table();
            x.header_version = Some(version);
            x.table.insert(1, Entry::gen0(Object::Null));
            x.max_obj_nr = 1;
            let mut info = Dict::default();
            info.set(b"Title", Object::String(b"T".to_vec()));
            let d = dict(&[
                ("Size", Object::Integer(2)),
                (
                    "Root",
                    Object::Ref(Ref {
                        num: 1,
                        generation: 0,
                    }),
                ),
                ("Info", Object::Dict(info)),
            ]);
            parse_trailer(&mut x, &d).unwrap();
            assert!(x.direct_info_dict.is_some());
            assert_eq!(x.info, None);
            materialize_direct_info_dict(&mut x).unwrap();
            assert!(x.direct_info_dict.is_none());
            if kept {
                assert_eq!(
                    x.info,
                    Some(Ref {
                        num: 2,
                        generation: 0
                    })
                );
                assert_eq!(x.table[&2].ref_count, 1);
                assert_eq!(x.size, Some(3));
                assert_eq!(x.max_obj_nr, 2);
            } else {
                assert_eq!(x.info, None);
                assert!(!x.exists(2));
            }
        }
    }

    #[test]
    fn materialize_allocates_past_the_highest_object_when_size_is_unset() {
        let mut x = table();
        x.header_version = Some(crate::model::Version::V20);
        x.max_obj_nr = 5;
        x.direct_info_dict = Some(Dict::default());
        materialize_direct_info_dict(&mut x).unwrap();
        assert_eq!(
            x.info,
            Some(Ref {
                num: 6,
                generation: 0
            })
        );
        assert_eq!(x.size, Some(7));
        // Without a header version Go dereferences nil.
        let mut x = table();
        x.direct_info_dict = Some(Dict::default());
        assert!(matches!(
            materialize_direct_info_dict(&mut x),
            Err(Error::Panic(_))
        ));
    }

    #[test]
    fn scan_trailer_collects_up_to_startxref() {
        let mem = Mem::new(&Limits::default());
        let mut s = Scanner::new(b"/Size 3 >>\nstartxref\n12\n%%EOF\n", lines);
        let buf = scan_trailer(&mem, &mut s, b"x << /Root 1 0 R").unwrap();
        assert_eq!(buf.v, b"<< /Root 1 0 R\n/Size 3 >>\n\n");
        // A trailer that never starts.
        let mut s = Scanner::new(b"nothing\n", lines);
        assert!(scan_trailer(&mem, &mut s, b"x").is_err());
    }

    #[test]
    fn xref_section_with_a_trailer_fills_the_table() {
        let input = b"xref\n0 2\n0000000000 65535 f \n0000000009 00000 n \ntrailer\n<</Size 2/Root 1 0 R>>\nstartxref\n0\n%%EOF\n";
        let mut ctx = ctx_for(input);
        let cancel = AtomicBool::new(false);
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        let mut s = Scanner::new(input, lines);
        let first = scan_line(&mut s).unwrap();
        assert_eq!(first, b"xref");
        let mut count = 0;
        let prev =
            parse_xref_section(&mut rcx, &mut ctx, &mut s, &[], &mut count, 0, 0, 1).unwrap();
        assert_eq!(prev, None);
        assert_eq!(count, 1);
        assert_eq!(ctx.xref.size, Some(2));
        assert_eq!(
            ctx.xref.root,
            Some(Ref {
                num: 1,
                generation: 0
            })
        );
        assert!(ctx.xref.table[&0].free);
        assert_eq!(ctx.xref.table[&1].offset, Some(9));
    }

    #[test]
    fn xref_section_without_a_trailer_is_missing_trailer_dict() {
        let input = b"0 1\n0000000000 65535 f \nbogus\n";
        let mut ctx = ctx_for(input);
        let cancel = AtomicBool::new(false);
        let mut rcx = ReadCx::new(input, &cancel, i64::MAX);
        let mut s = Scanner::new(input, lines);
        let mut count = 0;
        let err =
            parse_xref_section(&mut rcx, &mut ctx, &mut s, &[], &mut count, 0, 0, 1).unwrap_err();
        assert!(err.is(Sentinel::MissingTrailerDict), "{err}");
    }
}
