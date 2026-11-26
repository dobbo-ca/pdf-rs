// Licensed under the Apache License, Version 2.0 (see LICENSE.txt). A derivative work of
// pdf-go's pkg/pdfcpu (see NOTICE): the formats below mirror tests/parity/go/oracle/main.go.

//! The Rust side of the parity harness (docs/design.md section 12, docs/parity.md).
//!
//! `oracle [-threads N] <inputs> <outputs>` runs the read-side operations (read, pages, streams,
//! budget, encode) on every input of a directory, under the same limit configurations (d, g, s,
//! and b when `<name>.limits` exists), and writes the same files in the same formats as
//! `tests/parity/go/oracle`, which `-gen` writes the inputs for. `tests/parity/diff.sh` compares
//! the two trees. `-threads N` sets `max_threads`; the output must not depend on it.
//!
//! Each (input, configuration) is ONE file `<name>.<c>.txt` of sections ordered by op then kind,
//! each a header line `== <op> <kind>` and the content. An operation that fails as a whole has the
//! kind `err`, holding `Error::class()`. Raw bytes are written as `hs()`.
//!
//! WP 27 adds `images`, `validate`, `rvo`, `propsadd`, `props`, `write` and `writeopt`, and the
//! input-independent `build` op (`_build.g.txt`). The write and the properties use
//! `WriteEnv::parity()`, which the Go oracle's hooks match.
#![allow(clippy::needless_range_loop, clippy::chunks_exact_to_as_chunks)]

use pdf::config::{Cmd, Config, Limits};
use pdf::error::Error;
use pdf::filter::goflate::ZlibWriter;
use pdf::image::{color_space_components, extract_image};
use pdf::model::{Context, EntryObject, XRefTable};
use pdf::object::date::{date_string, date_time};
use pdf::object::text::{escaped_utf16_string, hex_literal_to_string, string_literal_to_string};
use pdf::object::write::array_pdf_string;
use pdf::object::{Dict, LazyRef, Object, Rect, StreamDict};
use pdf::{Bytes, Mem, WriteEnv};
use std::collections::BTreeMap;
use std::fmt::{Display, Write as _};
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// The most pages, streams and decoded bytes one input costs (main.go has the same constants).
const MAX_PAGES: i64 = 256;
const MAX_STREAMS: usize = 512;
const MAX_ENCODE_IN: usize = 4 << 20;
const MAX_IMAGES: usize = 64;

fn main() {
    if let Err(e) = run() {
        eprintln!("oracle: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut threads = None;
    if args.len() > 1 && args[0] == "-threads" {
        threads = Some(args[1].parse::<usize>().map_err(|e| e.to_string())?);
        args = args.split_off(2);
    }
    if args.len() != 2 {
        return Err("usage: oracle [-threads N] <input dir> <output dir>".to_string());
    }
    let (inp, out) = (Path::new(&args[0]), Path::new(&args[1]));
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;

    let mut names = Vec::new();
    for e in std::fs::read_dir(inp).map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        let name = e.file_name().to_string_lossy().into_owned();
        if e.path().is_file() && !name.ends_with(".limits") {
            names.push(name);
        }
    }
    names.sort();

    build(out, threads)?;
    for name in &names {
        let data = std::fs::read(inp.join(name)).map_err(|e| e.to_string())?;
        if name.ends_with(".txt") {
            // pkg/filter/testdata: the encode op only, once.
            let mut limits = Limits::default();
            if let Some(t) = threads {
                limits.max_threads = t;
            }
            let mem = Mem::new(&limits);
            let x = XRefTable::new(Config {
                limits,
                cmd: Cmd::Validate,
            });
            let r = encode_buf(&x, &mem, &data, &AtomicBool::new(false));
            put(out, name, "g", "e0", "out", &r)?;
            for level in [1, 9] {
                let r = zlib_level(&mem, &data, level);
                put(out, name, "g", "zlevel", &level.to_string(), &r)?;
            }
            flush(out, name, "g")?;
            continue;
        }
        let side = std::fs::read_to_string(inp.join(format!("{name}.limits"))).ok();
        for (cn, mut limits) in configs(side.as_deref())? {
            if let Some(t) = threads {
                limits.max_threads = t;
            }
            doc(out, name, &data, cn, limits)?;
            flush(out, name, cn)?;
        }
    }
    Ok(())
}

/// The limit configurations (design section 12): d, g and s, and b from the sidecar.
fn configs(side: Option<&str>) -> Result<Vec<(&'static str, Limits)>, String> {
    // d: byblos's defaults (byblos/internal/pdfdoc/pdfdoc.go:81-93). StreamBytes is 64 MiB,
    // Objects 200 000, and MaxParsedBytes is ObjectTableBytes(200 000) / 4 * 3 +
    // TotalDecodeBytes = 230 400 000 + 67 108 864.
    let d = Limits {
        max_decode_bytes: 64 << 20,
        max_total_decode_bytes: 64 << 20,
        max_object_bytes: 64 << 20,
        max_object_count: 200_000,
        max_object_stream_total_count: 200_000,
        max_parsed_bytes: 297_508_864,
        ..Limits::default()
    };
    // g: DefaultResourceLimits().
    let g = Limits::default();
    // s: tight limits.
    let s = Limits {
        max_decode_bytes: 64 << 10,
        max_total_decode_bytes: 256 << 10,
        max_parsed_bytes: 1 << 20,
        max_object_stream_total_count: 1000,
        max_stream_total_bytes: 1 << 20,
        max_image_pixels: 1_000_000,
        max_object_bytes: 1 << 20,
        ..Limits::default()
    };
    let mut out = vec![("d", d), ("g", g), ("s", s)];
    if let Some(side) = side {
        let mut b = Limits::default();
        for line in side.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let ok = line
                .split_once('=')
                .and_then(|(k, v)| Some((k, v.parse::<u64>().ok()?)))
                .is_some_and(|(k, v)| set_limit(&mut b, k, v));
            if !ok {
                return Err(format!("bad sidecar line {line:?}"));
            }
        }
        out.push(("b", b));
    }
    Ok(out)
}

/// Sets the field named `k` (the names are the Rust field names, and main.go's `setLimit` takes
/// the same).
fn set_limit(l: &mut Limits, k: &str, v: u64) -> bool {
    match k {
        "max_input_bytes" => l.max_input_bytes = v,
        "max_object_bytes" => l.max_object_bytes = v,
        "max_stream_bytes" => l.max_stream_bytes = v,
        "max_decode_bytes" => l.max_decode_bytes = v,
        "max_total_decode_bytes" => l.max_total_decode_bytes = v,
        "max_image_pixels" => l.max_image_pixels = v,
        "max_image_bytes" => l.max_image_bytes = v,
        "max_object_count" => l.max_object_count = v,
        "max_object_stream_count" => l.max_object_stream_count = v,
        "max_object_stream_total_count" => l.max_object_stream_total_count = v,
        "max_parsed_bytes" => l.max_parsed_bytes = v,
        "max_stream_total_bytes" => l.max_stream_total_bytes = v,
        "max_object_stream_first" => l.max_object_stream_first = v,
        "max_xref_entries" => l.max_xref_entries = v,
        "max_recursion_depth" => l.max_recursion_depth = v,
        _ => return false,
    }
    true
}

thread_local! {
    /// The sections `(op, kind, content)` of the (input, configuration) being run, until `flush`.
    static PENDING: std::cell::RefCell<Vec<(String, String, Vec<u8>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Adds the section `op` / `kind` to the file of the current (input, configuration).
fn put(
    _out: &Path,
    _name: &str,
    _cn: &str,
    op: &str,
    kind: &str,
    data: &[u8],
) -> Result<(), String> {
    PENDING.with(|p| {
        p.borrow_mut()
            .push((op.to_string(), kind.to_string(), data.to_vec()));
    });
    Ok(())
}

/// Writes the pending sections as ONE file `<name>.<cn>.txt`, ordered by op then kind (bytewise),
/// each as the header line `== <op> <kind>` and the content. Raw bytes are never written: the ops
/// that produce them put `hs(bytes)` instead.
fn flush(out: &Path, name: &str, cn: &str) -> Result<(), String> {
    let mut secs = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    secs.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let mut buf = Vec::new();
    for (op, kind, data) in secs {
        buf.extend_from_slice(format!("== {op} {kind}\n").as_bytes());
        buf.extend_from_slice(&data);
    }
    std::fs::write(out.join(format!("{name}.{cn}.txt")), buf).map_err(|e| e.to_string())
}

fn put_err(out: &Path, name: &str, cn: &str, op: &str, e: &Error) -> Result<(), String> {
    put(
        out,
        name,
        cn,
        op,
        "err",
        format!("{}\n", e.class()).as_bytes(),
    )
}

/// The operations of one input under one configuration. The document is read twice: once for
/// read, pages, streams and encode, and once, untouched, for budget, whose result depends on the
/// budget state a read leaves.
fn doc(out: &Path, name: &str, data: &[u8], cn: &str, limits: Limits) -> Result<(), String> {
    let cfg = Config {
        limits,
        cmd: Cmd::Validate,
    };
    let cancel = AtomicBool::new(false);
    let mut ctx = match pdf::api::read(data, &cfg, &cancel) {
        Ok(c) => c,
        Err(e) => return put_err(out, name, cn, "read", &e),
    };
    let dump = dump_read(&mut ctx, &cancel);
    put(out, name, cn, "read", "dump", &dump)?;
    let pages = op_pages(&mut ctx.xref, &cancel);
    put(out, name, cn, "pages", "out", &pages)?;
    streams(out, name, cn, &limits, &ctx.xref, &cancel)?;

    match pdf::api::read(data, &cfg, &cancel) {
        Ok(mut ctx2) => {
            let b = op_budget(&mut ctx2.xref, &cancel);
            put(out, name, cn, "budget", "out", &b)?;
        }
        Err(e) => put_err(out, name, cn, "budget", &e)?,
    }
    more_ops(out, name, data, cn, &cfg)
}

// ---------------------------------------------------------------------------------------------
// Formats

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut p = data.to_vec();
    p.push(0x80);
    while p.len() % 64 != 56 {
        p.push(0);
    }
    p.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
    for chunk in p.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (wi, c) in w.iter_mut().zip(chunk.chunks_exact(4)) {
            *wi = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let t1 = hh
                .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
                .wrapping_add((e & f) ^ (!e & g))
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let t2 = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                .wrapping_add((a & b) ^ (a & c) ^ (b & c));
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *s = s.wrapping_add(v);
        }
    }
    let mut out = [0u8; 32];
    for (o, v) in out.chunks_exact_mut(4).zip(h) {
        o.copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// `<len>:<sha256 hex>`.
fn hs(b: &[u8]) -> String {
    let mut s = format!("{}:", b.len());
    for x in sha256(b) {
        let _ = write!(s, "{x:02x}");
    }
    s
}

fn hs_opt(b: &Option<Bytes>) -> String {
    b.as_deref().map_or_else(|| "nil".to_string(), hs)
}

fn opt<T: Display>(v: Option<T>) -> String {
    v.map_or_else(|| "nil".to_string(), |v| v.to_string())
}

fn ref_str(r: Option<pdf::object::Ref>) -> String {
    r.map_or_else(
        || "nil".to_string(),
        |r| format!("{} {}", r.num, r.generation),
    )
}

/// A float as the shortest decimal that round-trips, with no exponent (Go's
/// `FormatFloat(f, 'f', -1, 64)`).
fn fmt_f(f: f64) -> String {
    if f.is_infinite() {
        let s = if f > 0.0 { "+Inf" } else { "-Inf" };
        return s.to_string();
    }
    f.to_string()
}

fn rect_str(r: Option<Rect>) -> String {
    r.map_or_else(
        || "nil".to_string(),
        |r| {
            format!(
                "{} {} {} {}",
                fmt_f(r.ll.x),
                fmt_f(r.ll.y),
                fmt_f(r.ur.x),
                fmt_f(r.ur.y)
            )
        },
    )
}

// ---------------------------------------------------------------------------------------------
// read

/// The canonical dump of a context (design section 12, op read).
fn dump_read(ctx: &mut Context, cancel: &AtomicBool) -> Vec<u8> {
    let mut b = Vec::new();
    {
        let t = &ctx.xref;
        let _ = writeln!(b, "size {}", opt(t.size));
        let _ = writeln!(b, "root {}", ref_str(t.root));
        let _ = writeln!(b, "info {}", ref_str(t.info));
        match &t.id {
            None => b.extend_from_slice(b"id nil\n"),
            Some(a) => {
                b.extend_from_slice(b"id ");
                b.extend_from_slice(&array_pdf_string(a));
                b.push(b'\n');
            }
        }
        let _ = writeln!(b, "encrypt {}", ref_str(t.encrypt));
        match &t.e {
            None => b.extend_from_slice(b"enc nil\n"),
            Some(e) => {
                let _ = writeln!(b, "enc p={} v={} r={}", e.p, e.v, e.r);
            }
        }
        let _ = writeln!(b, "header {}", opt(t.header_version));
        let _ = writeln!(b, "rootversion {}", opt(t.root_version));
        let _ = writeln!(b, "pagecount {}", t.page_count);
        let _ = writeln!(b, "maxobjnr {}", t.max_obj_nr);
        let using = ctx.read.as_ref().is_some_and(|r| r.using_object_streams);
        let _ = writeln!(b, "objstm {using}");
    }
    let keys: Vec<i64> = ctx.xref.table.keys().copied().collect();
    for k in keys {
        let lazy = {
            let Some(e) = ctx.xref.table.get(&k) else {
                continue;
            };
            let _ = write!(b, "obj {k} gen {} free {} ", opt(e.generation), e.free);
            match &e.object {
                None | Some(EntryObject::Obj(Object::Null)) => {
                    b.extend_from_slice(b"nil");
                    None
                }
                Some(EntryObject::Obj(Object::Stream(sd))) => {
                    b.extend_from_slice(b"stream ");
                    b.extend_from_slice(&sd.dict.pdf_string());
                    let _ = write!(
                        b,
                        " raw {} content {}",
                        hs_opt(&sd.raw),
                        hs_opt(&sd.content)
                    );
                    None
                }
                Some(EntryObject::Obj(o)) => {
                    b.extend_from_slice(&o.pdf_string());
                    None
                }
                Some(EntryObject::ObjStm(o)) => {
                    // No content: the lazy entries decode the stream on their own copy.
                    b.extend_from_slice(b"objstm ");
                    b.extend_from_slice(&o.stream.dict.pdf_string());
                    let _ = write!(
                        b,
                        " raw {} n {} first {}",
                        hs_opt(&o.stream.raw),
                        o.obj_count,
                        o.first_obj_offset
                    );
                    None
                }
                Some(EntryObject::XRefStm(x)) => {
                    b.extend_from_slice(b"xrefstm ");
                    b.extend_from_slice(&x.stream.dict.pdf_string());
                    let _ = write!(
                        b,
                        " raw {} size {} w {} {} {}",
                        hs_opt(&x.stream.raw),
                        x.size,
                        x.w[0],
                        x.w[1],
                        x.w[2]
                    );
                    None
                }
                Some(EntryObject::Lazy(l)) => Some(*l),
            }
        };
        if let Some(l) = lazy {
            lazy_text(&mut ctx.xref, cancel, l, &mut b);
        }
        b.push(b'\n');
    }
    b
}

/// `LazyObjectStreamObject.GetData`: the text of a lazy object. In a read context its
/// `decodeStreamFunc` decodes the object stream once through `DecodeObjectStream`, charging the
/// cumulative decode budget, and remembers the outcome for the later page reads.
fn lazy_text(x: &mut XRefTable, cancel: &AtomicBool, l: LazyRef, b: &mut Vec<u8>) {
    let r = x
        .lazy_content(l.stm, cancel)
        .and_then(|c| l.slice(&c).map(hs));
    match r {
        Ok(h) => {
            let _ = write!(b, "lazy {h}");
        }
        Err(e) => {
            let _ = write!(b, "lazy err {}", e.class());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// pages

/// `EnsurePageCount`, `PageDict(n, false)` and the inherited attributes for up to `MAX_PAGES`.
fn op_pages(x: &mut XRefTable, cancel: &AtomicBool) -> Vec<u8> {
    let mut b = Vec::new();
    if let Err(e) = x.ensure_page_count() {
        let _ = writeln!(b, "count err {}", e.class());
        return b;
    }
    let _ = writeln!(b, "count {}", x.page_count);
    for p in 1..=x.page_count.min(MAX_PAGES) {
        match x.page_dict(p, cancel) {
            Err(e) => {
                let _ = writeln!(b, "page {p} err {}", e.class());
            }
            Ok((d, r, a)) => {
                let _ = write!(b, "page {p} ref {} {} dict ", r.num, r.generation);
                b.extend_from_slice(&d.pdf_string());
                b.extend_from_slice(b" res ");
                match &a.resources {
                    Some(res) => b.extend_from_slice(&res.pdf_string()),
                    None => b.extend_from_slice(b"nil"),
                }
                let _ = writeln!(
                    b,
                    " mb {} cb {} rot {}",
                    rect_str(a.media_box),
                    rect_str(a.crop_box),
                    a.rotate
                );
            }
        }
    }
    b
}

// ---------------------------------------------------------------------------------------------
// streams, encode, budget

/// The object numbers of the plain stream dictionaries, ascending, up to `max`.
fn stream_numbers(x: &XRefTable, max: usize) -> Vec<i64> {
    x.table
        .iter()
        .filter(|(_, e)| matches!(e.object, Some(EntryObject::Obj(Object::Stream(_)))))
        .map(|(n, _)| *n)
        .take(max)
        .collect()
}

/// A copy of the stream of object `n` that has not been decoded.
fn fresh(x: &XRefTable, n: i64) -> Option<StreamDict> {
    match x.table.get(&n)?.object.as_ref()? {
        EntryObject::Obj(Object::Stream(sd)) => {
            let mut s = (**sd).clone();
            s.content = None;
            Some(s)
        }
        _ => None,
    }
}

/// `DecodeWithLimit` on a fresh copy of each stream, then `NewStreamDictForBuf` and `Encode` on
/// each decoded content.
fn streams(
    out: &Path,
    name: &str,
    cn: &str,
    limits: &Limits,
    x: &XRefTable,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mem = x.mem().clone();
    let lim = i64::try_from(limits.decode_limit()).unwrap_or(i64::MAX);
    for n in stream_numbers(x, MAX_STREAMS) {
        let Some(mut sd) = fresh(x, n) else {
            continue;
        };
        let op = format!("s{n}");
        if let Err(e) = sd.decode_with_limit(lim, &mem, cancel) {
            put(
                out,
                name,
                cn,
                &op,
                "out",
                format!("err {}\n", e.class()).as_bytes(),
            )?;
            continue;
        }
        let content: &[u8] = sd.content.as_deref().unwrap_or(&[]);
        put(
            out,
            name,
            cn,
            &op,
            "out",
            format!("ok {}\n", hs(content)).as_bytes(),
        )?;
        let cap = sd.content.as_ref().map_or(0, Bytes::go_cap);
        put(out, name, cn, &op, "cap", format!("{cap}\n").as_bytes())?;
        if content.len() <= MAX_ENCODE_IN {
            let r = encode_buf(x, &mem, content, cancel);
            put(out, name, cn, &format!("e{n}"), "out", &r)?;
        }
    }
    Ok(())
}

/// `NewStreamDictForBuf` and `Encode` on `data`, as a result line.
fn encode_buf(x: &XRefTable, mem: &Mem, data: &[u8], cancel: &AtomicBool) -> Vec<u8> {
    let r = mem
        .bytes_from(data, data.len())
        .and_then(|buf| -> Result<StreamDict, Error> {
            let mut sd = x.new_stream_dict_for_buf(buf);
            sd.encode(mem, cancel)?;
            Ok(sd)
        });
    match r {
        Err(e) => format!("err {}\n", e.class()).into_bytes(),
        Ok(sd) => format!(
            "raw {} length {}\n",
            hs(sd.raw.as_deref().unwrap_or(&[])),
            opt(sd.dict.int_entry(b"Length"))
        )
        .into_bytes(),
    }
}

/// `zlib.NewWriterLevel`, one `Write` and `Close` on `data`, as a result line (the zlevel op).
fn zlib_level(mem: &Mem, data: &[u8], level: i32) -> Vec<u8> {
    let mut z = Vec::new();
    let r = ZlibWriter::with_level(mem, level).and_then(|mut w| {
        let mut sink = |b: &[u8]| -> Result<(), Error> {
            z.extend_from_slice(b);
            Ok(())
        };
        w.write(data, &mut sink, &AtomicBool::new(false))?;
        w.close(&mut sink)
    });
    match r {
        Err(e) => format!("err {}\n", e.class()).into_bytes(),
        Ok(()) => format!("{}\n", hs(&z)).into_bytes(),
    }
}

/// `XRefTable.DecodeStream` over all plain streams in object order; the cumulative decode budget
/// carries from one stream to the next.
fn op_budget(x: &mut XRefTable, cancel: &AtomicBool) -> Vec<u8> {
    let mut b = Vec::new();
    for n in stream_numbers(x, usize::MAX) {
        let Some(mut sd) = fresh(x, n) else {
            continue;
        };
        match x.decode_stream(&mut sd, cancel) {
            Err(e) => {
                let _ = writeln!(b, "{n} {}", e.class());
            }
            Ok(()) => {
                let _ = writeln!(b, "{n} ok {}", sd.content.as_ref().map_or(0, Bytes::go_cap));
            }
        }
    }
    b
}

// ---------------------------------------------------------------------------------------------
// validate, rvo, propsadd, props, write, writeopt, images

/// `put` of the result of an operation, or its error class.
fn put_result(
    out: &Path,
    name: &str,
    cn: &str,
    op: &str,
    ext: &str,
    r: Result<Vec<u8>, Error>,
) -> Result<(), String> {
    match r {
        Ok(b) => put(out, name, cn, op, ext, &b),
        Err(e) => put_err(out, name, cn, op, &e),
    }
}

/// The operations that start from the bytes of the input. Each starts its own read, so none sees
/// the state another left behind (main.go's `moreOps`).
fn more_ops(out: &Path, name: &str, data: &[u8], cn: &str, cfg: &Config) -> Result<(), String> {
    let cancel = AtomicBool::new(false);
    let env = WriteEnv::parity();

    let r = pdf::api::validate(data, cfg, &cancel).map(|()| b"ok\n".to_vec());
    put_result(out, name, cn, "validate", "ok", r)?;

    let r = pdf::api::read_validate_and_optimize(data, cfg, &cancel).map(|mut ctx| {
        let mut b = dump_read(&mut ctx, &cancel);
        let names: Vec<&str> = ctx.xref.names.keys().map(String::as_str).collect();
        b.extend_from_slice(
            format!(
                "names {}\nform {}\n",
                names.join(","),
                ctx.xref.form.is_some()
            )
            .as_bytes(),
        );
        b
    });
    put_result(out, name, cn, "rvo", "dump", r)?;

    let r =
        pdf::api::read_and_validate(data, cfg, &cancel).map(|mut ctx| dump_read(&mut ctx, &cancel));
    put_result(out, name, cn, "rav", "dump", r)?;

    let add_cfg = Config {
        cmd: Cmd::AddProperties,
        ..*cfg
    };
    let props = BTreeMap::from([
        ("Title".to_string(), "parity".to_string()),
        ("Key".to_string(), "Wert \u{fc}".to_string()),
    ]);
    let r = pdf::api::read_validate_and_optimize(data, &add_cfg, &cancel).and_then(
        |mut ctx| -> Result<Vec<u8>, Error> {
            pdf::api::properties_add(&mut ctx, &props, &env, &cancel)?;
            Ok(dump_read(&mut ctx, &cancel))
        },
    );
    put_result(out, name, cn, "propsadd", "dump", r)?;

    let r = pdf::api::properties(data, cfg, &cancel).map(|m| props_text(&m));
    put_result(out, name, cn, "props", "out", r)?;

    op_write(out, name, data, cn, cfg, "write")?;
    op_write(out, name, data, cn, cfg, "writeopt")?;

    op_images(out, name, data, cn, cfg)
}

/// The sorted `k=v` lines of the properties, with backslash, newline and carriage return escaped.
fn props_text(m: &BTreeMap<String, String>) -> Vec<u8> {
    let esc = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    };
    let mut b = String::new();
    for (k, v) in m {
        let _ = writeln!(b, "{}={}", esc(k), esc(v));
    }
    b.into_bytes()
}

/// `ReadContext` (`write`) or `ReadValidateAndOptimize` (`writeopt`) then `WriteContext`: the exact
/// bytes (`<op>.out`) and the dump of a read of the output (`<op>.redump`, `err <class>` if that
/// read fails).
fn op_write(
    out: &Path,
    name: &str,
    data: &[u8],
    cn: &str,
    cfg: &Config,
    op: &str,
) -> Result<(), String> {
    let cancel = AtomicBool::new(false);
    let read = if op == "writeopt" {
        pdf::api::read_validate_and_optimize(data, cfg, &cancel)
    } else {
        pdf::api::read(data, cfg, &cancel)
    };
    let r =
        read.and_then(|mut ctx| pdf::api::write_context(&mut ctx, &WriteEnv::parity(), &cancel));
    let bytes = match r {
        Ok(b) => b,
        Err(e) => return put_err(out, name, cn, op, &e),
    };
    put(
        out,
        name,
        cn,
        op,
        "out",
        format!("{}\n", hs(&bytes)).as_bytes(),
    )?;
    let redump = match pdf::api::read(&bytes, cfg, &cancel) {
        Ok(mut ctx) => dump_read(&mut ctx, &cancel),
        Err(e) => format!("err {}\n", e.class()).into_bytes(),
    };
    put(out, name, cn, op, "redump", &redump)
}

/// The object numbers of the plain image streams, ascending.
fn image_numbers(x: &XRefTable) -> Vec<i64> {
    x.table
        .iter()
        .filter(|(_, e)| {
            matches!(&e.object, Some(EntryObject::Obj(Object::Stream(sd)))
                if sd.dict.subtype() == Some(b"Image".as_slice()))
        })
        .map(|(n, _)| *n)
        .collect()
}

/// `ColorSpaceComponents` and `ExtractImage` on a copy of each image stream (up to `MAX_IMAGES`,
/// ascending), after a read of its own. `images.out` has a line per image and
/// `i<obj>.<type>.out` the length and hash of each rendered file.
fn op_images(out: &Path, name: &str, data: &[u8], cn: &str, cfg: &Config) -> Result<(), String> {
    let cancel = AtomicBool::new(false);
    let mut ctx = match pdf::api::read(data, cfg, &cancel) {
        Ok(c) => c,
        Err(e) => return put_err(out, name, cn, "images", &e),
    };
    let mut b = Vec::new();
    for n in image_numbers(&ctx.xref).into_iter().take(MAX_IMAGES) {
        let Some(mut sd) = fresh(&ctx.xref, n) else {
            continue;
        };
        let _ = write!(b, "image {n} comps ");
        match color_space_components(&mut ctx.xref, &sd) {
            Ok(k) => {
                let _ = write!(b, "{k}");
            }
            Err(e) => {
                let _ = write!(b, "err {}", e.class());
            }
        }
        match extract_image(&mut ctx, &mut sd, format!("Im{n}").as_bytes(), n, &cancel) {
            Err(e) => {
                let _ = write!(b, " extract err {}", e.class());
            }
            Ok(None) => b.extend_from_slice(b" extract nil"),
            Ok(Some(im)) => {
                let h = hs(&im.data);
                let _ = write!(b, " extract {} {h}", im.file_type);
                let ext = format!("{}.out", im.file_type);
                put(
                    out,
                    name,
                    cn,
                    &format!("i{n}"),
                    &ext,
                    format!("{h}\n").as_bytes(),
                )?;
            }
        }
        let cap = sd.content.as_ref().map_or(0, Bytes::go_cap);
        let _ = writeln!(b, " cap {cap}");
    }
    put(out, name, cn, "images", "out", &b)
}

// ---------------------------------------------------------------------------------------------
// build

// The inputs of the text helpers; main.go has the same lists.
const BUILD_DATES: [&str; 10] = [
    "D:20240315123045+02'00'",
    "D:20240315",
    "20240315123045Z",
    "D:2024",
    "D:20241301000000Z",
    "not a date",
    "D:20240229235959-08'00",
    "D:19991231235959.5Z",
    "2024-03-15 12:30",
    "",
];
const BUILD_LITS: [&str; 6] = [
    "abc",
    r"a\(b\)c",
    r"\376\377\000H\000i",
    r"\357\273\277\303\244",
    r"\344\366",
    r"\376\377\000",
];
const BUILD_HEXES: [&str; 6] = ["48656C6C6F", "FEFF00480069", "EFBBBFC3A4", "4G", "", "4"];

/// The hex text of `b`.
fn hx(b: &[u8]) -> String {
    let mut s = String::new();
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// `ok <hex>` or `err <class>`.
fn text_or_err(r: Result<String, Error>) -> String {
    match r {
        Ok(s) => format!("ok {}", hx(s.as_bytes())),
        Err(e) => format!("err {}", e.class()),
    }
}

/// The entry points that byblos uses to make a document, and the text helpers of `types`
/// (main.go's `build`): `_build.g.build.out`, `_build.g.write.out` and `_build.g.text.out`.
fn build(out: &Path, threads: Option<usize>) -> Result<(), String> {
    const NAME: &str = "_build";
    match build_doc(threads) {
        Ok((lines, pdf)) => {
            put(
                out,
                NAME,
                "g",
                "write",
                "out",
                format!("{}\n", hs(&pdf)).as_bytes(),
            )?;
            put(out, NAME, "g", "build", "out", &lines)?;
        }
        Err(e) => put_err(out, NAME, "g", "build", &e)?,
    }
    put(out, NAME, "g", "text", "out", &build_text())?;
    flush(out, NAME, "g")
}

fn build_doc(threads: Option<usize>) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let cancel = AtomicBool::new(false);
    let mut limits = Limits::default();
    if let Some(t) = threads {
        limits.max_threads = t;
    }
    let cfg = Config {
        limits,
        cmd: Cmd::Validate,
    };
    let missing = |what: &str| Error::Corrupt(format!("build: {what} missing"));
    let mut b = Vec::new();

    let mut x = pdf::api::create_xref_table_with_root_dict(&cfg.limits)?;
    let _ = writeln!(
        b,
        "create ver {} size {} root {}",
        x.version_string()?,
        opt(x.size),
        ref_str(x.root)
    );
    let mem = x.mem().clone();

    let mut pages = Dict::default();
    pages.insert_name(b"Type", b"Pages");
    pages.insert_int(b"Count", 1);
    pages.insert(b"Kids", Object::Array(Vec::new()));
    let pages_ref = x.ind_ref_for_new_object(Object::Dict(pages))?;

    let text = b"0 0 m 100 100 l S\n";
    let mut sd = x.new_stream_dict_for_buf(mem.bytes_from(text, text.len())?);
    sd.encode(&mem, &cancel)?;
    let content_ref = x.ind_ref_for_new_object(Object::Stream(Box::new(sd)))?;

    let media_box = Object::Array(vec![
        Object::Integer(0),
        Object::Integer(0),
        Object::Integer(100),
        Object::Integer(100),
    ]);
    let mut page = Dict::default();
    page.insert_name(b"Type", b"Page");
    page.insert(b"Parent", Object::Ref(pages_ref));
    page.insert(b"MediaBox", media_box.clone());
    page.insert(b"Resources", Object::Dict(Dict::default()));
    page.insert(b"Contents", Object::Ref(content_ref));
    let page_ref = x.ind_ref_for_new_object(Object::Dict(page))?;

    x.dereference_dict_mut(pages_ref)?
        .ok_or_else(|| missing("pages"))?
        .set(b"Kids", Object::Array(vec![Object::Ref(page_ref)]));
    x.catalog_mut()?.insert(b"Pages", Object::Ref(pages_ref));
    let obj_nr = x.insert_object(Object::Integer(7))?;
    let _ = writeln!(b, "inserted {obj_nr} size {}", opt(x.size));
    let _ = writeln!(b, "rect {}", rect_str(Some(Rect::for_dim(595.5, 842.0))));

    let mut ctx = pdf::api::create_context(x, cfg);
    let x = &mut ctx.xref;
    b.extend_from_slice(b"catalog ");
    b.extend_from_slice(&x.catalog()?.pdf_string());
    b.push(b'\n');
    let pr = x.pages()?;
    let _ = writeln!(b, "pages {}", ref_str(Some(pr)));
    for n in [0, 1, 3, obj_nr, 99] {
        let e = x.find_table_entry(n, 0);
        let _ = write!(b, "entry {n} found {}", e.is_some());
        if let Some(e) = e {
            let _ = write!(
                b,
                " gen {} free {} refcount {}",
                opt(e.generation),
                e.free,
                e.ref_count
            );
        }
        b.push(b'\n');
    }
    let _ = writeln!(
        b,
        "entry page found {}",
        x.find_table_entry_for_ind_ref(page_ref).is_some()
    );
    let mb = x
        .dereference_array(&media_box)?
        .ok_or_else(|| missing("media box"))?;
    b.extend_from_slice(b"mediabox ");
    b.extend_from_slice(&array_pdf_string(mb));
    b.push(b'\n');
    let page_obj = Object::Ref(page_ref);
    b.extend_from_slice(b"page ");
    b.extend_from_slice(&x.dereference(&page_obj)?.pdf_string());
    b.push(b'\n');
    b.extend_from_slice(b"pagesdict ");
    let d = x
        .dereference_dict_ref(pages_ref)?
        .ok_or_else(|| missing("pages dict"))?;
    b.extend_from_slice(&d.pdf_string());
    b.push(b'\n');
    let (csd, _) = x.dereference_stream_dict(&Object::Ref(content_ref))?;
    let csd = csd.ok_or_else(|| missing("content stream"))?;
    b.extend_from_slice(b"content ");
    b.extend_from_slice(&csd.dict.pdf_string());
    let _ = writeln!(
        b,
        " raw {} length {}",
        hs(csd.raw.as_deref().unwrap_or(&[])),
        opt(csd.dict.int_entry(b"Length"))
    );

    let pdf = pdf::api::write_context(&mut ctx, &WriteEnv::parity(), &cancel)?;
    let _ = writeln!(b, "write {}", hs(&pdf));
    Ok((b, pdf))
}

fn build_text() -> Vec<u8> {
    let mut b = Vec::new();
    for s in BUILD_DATES {
        match date_time(s.as_bytes(), true) {
            None => {
                let _ = writeln!(b, "date {} bad", hx(s.as_bytes()));
            }
            Some(t) => {
                let _ = writeln!(b, "date {} {}", hx(s.as_bytes()), date_string(&t));
            }
        }
    }
    for s in BUILD_LITS {
        let r = string_literal_to_string(s.as_bytes());
        let _ = writeln!(b, "lit {} {}", hx(s.as_bytes()), text_or_err(r));
    }
    for s in BUILD_HEXES {
        let r = hex_literal_to_string(s.as_bytes());
        let _ = writeln!(b, "hex {} {}", hx(s.as_bytes()), text_or_err(r));
    }
    let escapes: [&[u8]; 6] = [
        b"plain",
        "Wert \u{fc}".as_bytes(),
        "\u{65e5}\u{672c}\u{8a9e}".as_bytes(),
        br"a(b)c\",
        b"",
        b"\xff",
    ];
    for s in escapes {
        match escaped_utf16_string(s) {
            Ok(r) => {
                let _ = writeln!(b, "escape {} ok {}", hx(s), hx(&r));
            }
            Err(e) => {
                let _ = writeln!(b, "escape {} err {}", hx(s), e.class());
            }
        }
    }
    b
}
