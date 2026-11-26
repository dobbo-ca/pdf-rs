//! Measures the real heap peak of the public entry points against `Limits::max_alloc` (round-c
//! item 6). A counting `#[global_allocator]` (std only) tracks the live and the peak heap bytes;
//! each measurement resets the peak to the live bytes at that moment, so the input and the test's
//! own buffers do not count. `tests/fuzz/common.rs` compares `Mem::peak()` with `max_alloc`,
//! which cannot fail (the ledger refuses a charge over the limit before it records it): this test
//! sees allocations the ledger never charges.
//!
//! The documented contract is that `max_alloc` is the peak of one document plus the call in
//! progress. The assert is `peak <= max_alloc + SLACK`, whether the call succeeds or fails with a
//! limit error: a refused charge must not follow an uncharged allocation of that size.
//!
//! A shared ledger (`api::read_with_mem`) holds two documents to the same bound.
//!
//! Everything runs in one `#[test]`, because the allocator counters are global and the test
//! harness would otherwise run several tests at once.

use pdf::crypto::{decrypt_key, validate_user_password};
use pdf::model::EntryObject;
use pdf::model::xref::Enc;
use pdf::object::Object;
use pdf::object::string::escape;
use pdf::{Config, Error, Limits, Mem, WriteEnv, api};
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Heap that is not charged to the ledger by design: the flate reader's window (256 KiB per
/// `READER_BYTES`, src/filter/flate.rs), nested-crate scratch, the `Context` shell, `Vec` growth
/// where the old and the new buffer coexist during a `realloc`, and the test harness's own
/// allocations on other threads. 4 MiB is far below the amplification this test hunts (a 20 KB
/// input that costs hundreds of MiB), so it does not hide a finding.
const SLACK: u64 = 4 << 20;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

fn add(n: usize) {
    let now = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: every method forwards to `System` with the caller's arguments and only adds counting.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract, which `System` shares.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as in `alloc`.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` and `layout` come from this allocator, which forwards to `System`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as in `dealloc`; `new_size` is the caller's.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                add(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Runs `f` and returns its result with the most heap bytes that were live at once above the
/// live bytes at the start. The result is still alive when the peak is read.
fn measure<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let r = f();
    (r, PEAK.load(Ordering::Relaxed).saturating_sub(base) as u64)
}

type Op = fn(&[u8], &Config) -> Result<(), Error>;

fn op_read(d: &[u8], c: &Config) -> Result<(), Error> {
    api::read(d, c, &AtomicBool::new(false)).map(|_| ())
}

fn op_read_and_validate(d: &[u8], c: &Config) -> Result<(), Error> {
    api::read_and_validate(d, c, &AtomicBool::new(false)).map(|_| ())
}

fn op_read_validate_and_optimize(d: &[u8], c: &Config) -> Result<(), Error> {
    api::read_validate_and_optimize(d, c, &AtomicBool::new(false)).map(|_| ())
}

/// Read, validate and optimize, then write: the document and the output are live together.
fn op_write_context(d: &[u8], c: &Config) -> Result<(), Error> {
    let cancel = AtomicBool::new(false);
    let mut ctx = api::read_validate_and_optimize(d, c, &cancel)?;
    api::write_context(&mut ctx, &WriteEnv::parity(), &cancel).map(|_| ())
}

/// `extract_image` on a copy of each of the first four image streams.
fn op_extract_image(d: &[u8], c: &Config) -> Result<(), Error> {
    let cancel = AtomicBool::new(false);
    let mut ctx = api::read(d, c, &cancel)?;
    let numbers: Vec<i64> = ctx
        .xref
        .table
        .iter()
        .filter(|(_, e)| {
            matches!(&e.object, Some(EntryObject::Obj(Object::Stream(sd)))
                if sd.dict.subtype() == Some(b"Image".as_slice()))
        })
        .map(|(n, _)| *n)
        .take(4)
        .collect();
    for n in numbers {
        let Some(EntryObject::Obj(Object::Stream(sd))) =
            ctx.xref.table.get(&n).and_then(|e| e.object.as_ref())
        else {
            continue;
        };
        let mut sd = (**sd).clone();
        sd.content = None;
        // The peak is the assert; an error (an unsupported filter, a limit) is not this test's concern.
        let _ = api::extract_image(&mut ctx, &mut sd, b"Im0", n, &cancel);
    }
    Ok(())
}

const DOCUMENT_OPS: [(&str, Op); 4] = [
    ("read", op_read),
    ("read_and_validate", op_read_and_validate),
    ("read_validate_and_optimize", op_read_validate_and_optimize),
    ("write_context", op_write_context),
];

/// Asserts the peak of `op` on `data` at `max_alloc`.
fn check(name: &str, what: &str, op: Op, data: &[u8], max_alloc: u64) {
    let cfg = Config {
        limits: Limits {
            max_alloc,
            ..Limits::default()
        },
        ..Config::default()
    };
    let (r, peak) = measure(|| op(data, &cfg));
    let outcome = match &r {
        Ok(()) => "ok".to_string(),
        Err(e) => e.to_string(),
    };
    assert!(
        peak <= max_alloc + SLACK,
        "{name}: {what} at max_alloc {max_alloc}: heap peak {peak} is over max_alloc + {SLACK} (result: {outcome})"
    );
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// A classic PDF of `objects` (numbers 1..); object 1 is the catalog. `trailer` adds entries to
/// the trailer.
fn classic_pdf(objects: &[Vec<u8>], trailer: &str) -> Vec<u8> {
    let mut f = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(f.len());
        f.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        f.extend_from_slice(o);
        f.extend_from_slice(b"\nendobj\n");
    }
    let xref = f.len();
    f.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for o in offsets {
        f.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    f.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R {trailer} >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    f
}

/// A zlib stream of `n` zero bytes, by hand: one fixed-Huffman block, a literal 0 and then
/// length-258 distance-1 matches. About 13 bits per 258 bytes.
fn zlib_zeros(n: usize) -> Vec<u8> {
    struct Bits {
        out: Vec<u8>,
        acc: u32,
        n: u32,
    }
    impl Bits {
        // One bit at a time, so a Huffman code goes in most significant bit first.
        fn code(&mut self, code: u32, len: u32) {
            for i in (0..len).rev() {
                self.acc |= ((code >> i) & 1) << self.n;
                self.n += 1;
                if self.n == 8 {
                    self.out.push(self.acc as u8);
                    self.acc = 0;
                    self.n = 0;
                }
            }
        }
    }
    let mut b = Bits {
        out: vec![0x78, 0x9c],
        // BFINAL = 1, BTYPE = 01 (fixed Huffman), least significant bit first.
        acc: 0b011,
        n: 3,
    };
    let mut left = n;
    if left > 0 {
        b.code(0x30, 8); // literal 0
        left -= 1;
    }
    while left >= 258 {
        b.code(0xc5, 8); // length symbol 285 = 258
        b.code(0, 5); // distance symbol 0 = 1
        left -= 258;
    }
    for _ in 0..left {
        b.code(0x30, 8);
    }
    b.code(0, 7); // end of block
    if b.n > 0 {
        b.out.push(b.acc as u8);
    }
    let adler = (((n % 65521) as u32) << 16) | 1;
    b.out.extend_from_slice(&adler.to_be_bytes());
    b.out
}

/// A PDF whose only cross-reference section is a stream with `/Size n` and `n` free entries
/// (`/W [1 0 0]`, all zero): a few tens of KB of input that a reader may expand into `n` table
/// entries.
fn big_xref_stream_pdf(n: usize) -> Vec<u8> {
    let z = zlib_zeros(n);
    let mut f = b"%PDF-1.7\n".to_vec();
    f.extend_from_slice(b"2 0 obj\n<< /Type /Catalog /Pages 3 0 R >>\nendobj\n");
    f.extend_from_slice(b"3 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
    let at = f.len();
    f.extend_from_slice(
        format!(
            "1 0 obj\n<< /Type /XRef /Size {n} /W [1 0 0] /Root 2 0 R /Filter /FlateDecode /Length {} >>\nstream\n",
            z.len()
        )
        .as_bytes(),
    );
    f.extend_from_slice(&z);
    f.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{at}\n%%EOF\n").as_bytes());
    f
}

/// A PDF with one array object of `n` elements, 2 input bytes each; the catalog refers to it.
fn huge_array_pdf(n: usize) -> Vec<u8> {
    let mut a = b"[".to_vec();
    a.extend(std::iter::repeat_n(*b"0 ", n).flatten());
    a.push(b']');
    classic_pdf(
        &[
            b"<< /Type /Catalog /Pages 2 0 R /Extra 3 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            a,
        ],
        "",
    )
}

/// A PDF without `startxref`, so the reader repairs it: a catalog, a page root and `n` null
/// objects, each a table entry.
fn repair_pdf(n: usize) -> Vec<u8> {
    let mut f = b"%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec();
    f.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
    for i in 3..n + 3 {
        f.extend_from_slice(format!("{i} 0 obj\nnull\nendobj\n").as_bytes());
    }
    f
}

/// A page root with `n` null kids. Validation copies the root and its kids; `n` is chosen so that
/// the parse fits a limit and the copies do not.
fn null_kids_pdf(n: usize) -> Vec<u8> {
    let mut root = b"<< /Type /Pages /Count 0 /Kids [".to_vec();
    root.extend(std::iter::repeat_n(*b"null ", n).flatten());
    root.extend_from_slice(b"] >>");
    classic_pdf(&[b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), root], "")
}

/// A dictionary object of `n` entries whose keys come in order, which leaves the B-tree nodes
/// about half full; the catalog refers to it.
fn big_dict_pdf(n: usize) -> Vec<u8> {
    let mut d = b"<<".to_vec();
    for i in 0..n {
        d.extend_from_slice(format!("/k{i:07} 0").as_bytes());
    }
    d.extend_from_slice(b">>");
    classic_pdf(
        &[
            b"<< /Type /Catalog /Pages 2 0 R /Extra 3 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            d,
        ],
        "",
    )
}

/// [`big_dict_pdf`] with a `%PDF-2.0` header, so the writer checks each dictionary it reaches
/// for an annotation.
fn big_dict_pdf20(n: usize) -> Vec<u8> {
    let mut f = big_dict_pdf(n);
    f[..8].copy_from_slice(b"%PDF-2.0");
    f
}

/// A PDF whose catalog refers to an empty stream with the stream dictionary `dict`.
fn stream_pdf(dict: &[u8]) -> Vec<u8> {
    let mut s = dict.to_vec();
    s.extend_from_slice(b"\nstream\n\nendstream");
    classic_pdf(
        &[
            b"<< /Type /Catalog /Pages 2 0 R /Extra 3 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            s,
        ],
        "",
    )
}

/// A stream whose `/Filter` is `n` times `/FlateDecode`: the reader builds a filter pipeline of
/// `n` stages.
fn filter_array_pdf(n: usize) -> Vec<u8> {
    let mut d = b"<< /Length 0 /Filter [".to_vec();
    d.extend(std::iter::repeat_n(b"/FlateDecode ".as_slice(), n).flatten());
    d.extend_from_slice(b"] >>");
    stream_pdf(&d)
}

/// A stream whose one `/DecodeParms` dictionary has `n` entries, which the reader copies into the
/// filter pipeline.
fn big_decode_parms_pdf(n: usize) -> Vec<u8> {
    use std::io::Write;
    let mut d = b"<< /Length 0 /Filter /FlateDecode /DecodeParms <<".to_vec();
    for i in 0..n {
        write!(d, "/k{i:07} 0").unwrap();
    }
    d.extend_from_slice(b">> >>");
    stream_pdf(&d)
}

/// A PDF whose `/Dests` name tree is one leaf of `n` keys with null values: validation copies
/// the `/Names` array and keeps copies of it in the tree it builds.
fn name_tree_pdf(n: usize) -> Vec<u8> {
    let mut a = b"<< /Names [".to_vec();
    for i in 0..n {
        a.extend_from_slice(format!("(k{i:07}) null ").as_bytes());
    }
    a.extend_from_slice(b"] >>");
    classic_pdf(
        &[
            b"<< /Type /Catalog /Pages 2 0 R /Names << /Dests 3 0 R >> >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            a,
        ],
        "",
    )
}

/// A page tree root whose `/Resources` hold `n` entries (under `/Encoding`, which validation
/// keeps and does not check) and `pages` pages without content: the optimizer copies the
/// inherited resources into every page.
fn inherited_resources_pdf(n: usize, pages: usize) -> Vec<u8> {
    let mut res = b"<< /Encoding <<".to_vec();
    for i in 0..n {
        res.extend_from_slice(format!("/k{i:07} 0").as_bytes());
    }
    res.extend_from_slice(b">> >>");
    let mut root =
        format!("<< /Type /Pages /MediaBox [0 0 1 1] /Resources 3 0 R /Count {pages} /Kids [")
            .into_bytes();
    for i in 0..pages {
        root.extend_from_slice(format!("{} 0 R ", i + 4).as_bytes());
    }
    root.extend_from_slice(b"] >>");
    let mut objects = vec![b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), root, res];
    objects.extend(std::iter::repeat_n(
        b"<< /Type /Page /Parent 2 0 R >>".to_vec(),
        pages,
    ));
    classic_pdf(&objects, "")
}

/// A PDF whose catalog refers to a string object of `n` bytes, which the writer copies and
/// serialises.
fn big_string_pdf(n: usize) -> Vec<u8> {
    let mut s = b"(".to_vec();
    s.resize(n + 1, b'a');
    s.push(b')');
    classic_pdf(
        &[
            b"<< /Type /Catalog /Pages 2 0 R /Extra 3 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            s,
        ],
        "",
    )
}

/// A PDF whose catalog holds a direct string of `n` bytes, which the writer copies with the
/// catalog and again for each entry it follows.
fn big_catalog_string_pdf(n: usize) -> Vec<u8> {
    let mut c = b"<< /Type /Catalog /Pages 2 0 R /X (".to_vec();
    c.resize(c.len() + n, b'a');
    c.extend_from_slice(b") >>");
    classic_pdf(&[c, b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec()], "")
}

/// A catalog dictionary with `n` extra entries: validation copies the catalog and its parts.
fn big_catalog_dict_pdf(n: usize) -> Vec<u8> {
    let mut c = b"<< /Type /Catalog /Pages 2 0 R".to_vec();
    for i in 0..n {
        c.extend_from_slice(format!("/k{i:07} 0").as_bytes());
    }
    c.extend_from_slice(b" >>");
    classic_pdf(&[c, b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec()], "")
}

/// `pages` pages that share one `/Annots` array of `n` references to one text annotation:
/// validation records every entry for every page and stores a cleaned copy of the array in each.
fn shared_annots_pdf(n: usize, pages: usize) -> Vec<u8> {
    let mut root =
        format!("<< /Type /Pages /MediaBox [0 0 1 1] /Count {pages} /Kids [").into_bytes();
    for i in 0..pages {
        root.extend_from_slice(format!("{} 0 R ", i + 5).as_bytes());
    }
    root.extend_from_slice(b"] >>");
    let mut annots = b"[".to_vec();
    annots.extend(std::iter::repeat_n(b"4 0 R ".as_slice(), n).flatten());
    annots.push(b']');
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        root,
        annots,
        b"<< /Type /Annot /Subtype /Text /Rect [0 0 1 1] >>".to_vec(),
    ];
    objects.extend(std::iter::repeat_n(
        b"<< /Type /Page /Parent 2 0 R /Annots 3 0 R >>".to_vec(),
        pages,
    ));
    classic_pdf(&objects, "")
}

/// A PDF without `startxref` whose catalog holds a string of `n` bytes: the repair keeps a copy
/// of the catalog and compares it with the table's.
fn repaired_big_catalog_pdf(n: usize) -> Vec<u8> {
    let mut f = b"%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R /X (".to_vec();
    f.resize(f.len() + n, b'z');
    f.extend_from_slice(b") >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
    f
}

/// RC4.
fn rc4(key: &[u8], data: &mut [u8]) {
    let mut s: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for (i, k) in key.iter().cycle().take(256).enumerate() {
        j = j.wrapping_add(s[i]).wrapping_add(*k);
        s.swap(i, usize::from(j));
    }
    let (mut i, mut j) = (0u8, 0u8);
    for b in data {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[usize::from(i)]);
        s.swap(usize::from(i), usize::from(j));
        *b ^= s[usize::from(s[usize::from(i)].wrapping_add(s[usize::from(j)]))];
    }
}

/// An RC4 40-bit file (revision 2, empty passwords) whose object 3 is an array of `count` literal
/// strings that decrypt to `len` backslashes each. The reader escapes them again, to twice their
/// length; just over a power of two, the escaped buffer's capacity is four times the input.
fn escaped_strings_pdf(count: usize, len: usize) -> Vec<u8> {
    const PAD: [u8; 32] = [
        0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01,
        0x08, 0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53,
        0x69, 0x7A,
    ];
    let e = Enc {
        o: vec![0; 32],
        p: -4,
        r: 2,
        v: 1,
        l: 40,
        id: vec![1, 2, 3, 4, 5],
        ..Enc::default()
    };
    // The file key of the empty user password (`/U` is still empty, so the check fails).
    let mut key = Vec::new();
    validate_user_password(&e, b"", &mut key).unwrap();
    let mut u = PAD.to_vec();
    rc4(&key, &mut u);
    let obj_key = decrypt_key(3, 0, &key, false).unwrap();
    let mut a = b"[".to_vec();
    for _ in 0..count {
        let mut c = vec![b'\\'; len];
        rc4(&obj_key, &mut c);
        a.push(b'(');
        a.extend(escape(&c));
        a.extend_from_slice(b") ");
    }
    a.push(b']');
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    classic_pdf(
        &[
            b"<< /Type /Catalog /Pages 2 0 R /Extra 3 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            a,
            format!(
                "<< /Filter /Standard /V 1 /R 2 /O <{}> /U <{}> /P -4 >>",
                hex(&e.o[..]),
                hex(&u[..])
            )
            .into_bytes(),
        ],
        "/Encrypt 4 0 R /ID [<0102030405> <0102030405>]",
    )
}

/// A page whose content uses `n` distinct XObject names, after an operator glued to the next
/// word (`DoQ`): the consolidation copies the rest of the content once and collects the names.
fn content_names_pdf(n: usize) -> Vec<u8> {
    let mut c = b"/x DoQ\n".to_vec();
    for i in 0..n {
        c.extend_from_slice(format!("/n{i:07} Do\n").as_bytes());
    }
    let mut s = format!("<< /Length {} >>\nstream\n", c.len()).into_bytes();
    s.extend_from_slice(&c);
    s.extend_from_slice(b"\nendstream");
    classic_pdf(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] /Contents 4 0 R >>".to_vec(),
            s,
        ],
        "",
    )
}

/// `pages` pages that inherit one `XObject` dictionary of `forms` form XObjects of distinct
/// lengths: the optimizer records each form as visited once per page.
fn shared_forms_pdf(forms: usize, pages: usize) -> Vec<u8> {
    let mut xobjects = b"<<".to_vec();
    for i in 0..forms {
        xobjects.extend_from_slice(format!("/F{i:05} {} 0 R ", i + 4).as_bytes());
    }
    xobjects.extend_from_slice(b">>");
    let first_page = forms + 4;
    let mut root = format!(
        "<< /Type /Pages /MediaBox [0 0 1 1] /Resources << /XObject 3 0 R >> /Count {pages} /Kids ["
    )
    .into_bytes();
    for i in 0..pages {
        root.extend_from_slice(format!("{} 0 R ", first_page + i).as_bytes());
    }
    root.extend_from_slice(b"] >>");
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        root,
        xobjects,
    ];
    for i in 0..forms {
        let mut f = format!(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 1 1] /Length {} >>\nstream\n",
            i + 1
        )
        .into_bytes();
        f.resize(f.len() + i + 1, b' ');
        f.extend_from_slice(b"\nendstream");
        objects.push(f);
    }
    objects.extend(std::iter::repeat_n(
        b"<< /Type /Page /Parent 2 0 R >>".to_vec(),
        pages,
    ));
    classic_pdf(&objects, "")
}

#[test]
fn heap_peak_stays_within_max_alloc_plus_slack() {
    let small = [
        "test.pdf",
        "testWithText.pdf",
        "zineTest.pdf",
        "bookletTestA6.pdf",
        "Walden.pdf",
    ];
    let images = ["testImage.pdf", "blank-scan.pdf"];
    let adversarial = [
        ("big xref stream", big_xref_stream_pdf(2_000_000)),
        ("huge array", huge_array_pdf(2_000_000)),
        // About 100 MB of uncharged table entries without the repair path's charge.
        ("repair", repair_pdf(500_000)),
        // 15 MB of parse estimate at 16 MiB, and two copies of 7.5 MB in validation.
        ("null kids", null_kids_pdf(235_000)),
        // About 10% over the old per-entry estimate at 256 MiB.
        ("big dict", big_dict_pdf(2_400_000)),
        // 7.3 MB of strings that become 29 MB of escaped buffers at 16 MiB.
        ("escaped strings", escaped_strings_pdf(14, (1 << 19) + 1)),
    ];
    for max_alloc in [1u64 << 20, 16 << 20, 256 << 20] {
        for name in small {
            let data = fixture(name);
            for (what, op) in DOCUMENT_OPS {
                check(name, what, op, &data, max_alloc);
            }
        }
        for name in images {
            check(
                name,
                "extract_image",
                op_extract_image,
                &fixture(name),
                max_alloc,
            );
        }
        for (name, data) in &adversarial {
            for (what, op) in DOCUMENT_OPS {
                check(name, what, op, data, max_alloc);
            }
            check(name, "extract_image", op_extract_image, data, max_alloc);
        }
    }
    // The writer's copy and text of a 20 MB string, and of an encrypted 2 MB one (4 MB escaped).
    let big_string = big_string_pdf(20_000_000);
    let encrypted = escaped_strings_pdf(1, 2_000_000);
    for max_alloc in [16u64 << 20, 64 << 20] {
        check(
            "big string",
            "write_context",
            op_write_context,
            &big_string,
            max_alloc,
        );
        check(
            "encrypted string",
            "write_context",
            op_write_context,
            &encrypted,
            max_alloc,
        );
    }
    check(
        "big catalog string",
        "write_context",
        op_write_context,
        &big_catalog_string_pdf(20_000_000),
        64 << 20,
    );
    check(
        "repaired big catalog",
        "read",
        op_read,
        &repaired_big_catalog_pdf(3_000_000),
        16 << 20,
    );
    // The PDF 2.0 annotation check of a dictionary that the read charges at about 0.9 of the limit.
    for (n, max_alloc) in [(1_700_000, 256u64 << 20), (106_000, 16 << 20)] {
        check(
            "big dict 2.0",
            "write_context",
            op_write_context,
            &big_dict_pdf20(n),
            max_alloc,
        );
    }
    // A filter pipeline of 200,000 stages, and one whose parameter dictionary the read charges at
    // about 0.9 of the limit: the pipeline is kept in the table and copied by the writer.
    let filters = filter_array_pdf(200_000);
    let parms = big_decode_parms_pdf(1_800_000);
    for (what, op) in [("read", op_read as Op), ("write_context", op_write_context)] {
        check("filter array", what, op, &filters, 16 << 20);
        check("big decode parms", what, op, &parms, 256 << 20);
    }
    // Validation's copies of a name tree of 600,000 keys, three of which it keeps in the table.
    check(
        "name tree",
        "read_and_validate",
        op_read_and_validate,
        &name_tree_pdf(600_000),
        256 << 20,
    );
    // Validation's working copies of a catalog of 400,000 entries (about 50 MB), at a limit just
    // above the read's peak: the working-copy reserve refuses them before they are made.
    let catalog = big_catalog_dict_pdf(400_000);
    let read = api::read(&catalog, &Config::default(), &AtomicBool::new(false)).unwrap();
    let max_alloc = read.xref.mem().peak() + (1 << 20);
    drop(read);
    check(
        "big catalog dict",
        "read_and_validate",
        op_read_and_validate,
        &catalog,
        max_alloc,
    );
    let cfg = Config {
        limits: Limits {
            max_alloc,
            ..Limits::default()
        },
        ..Config::default()
    };
    let e = op_read_and_validate(&catalog, &cfg).unwrap_err();
    assert!(
        matches!(e.root(), Error::Alloc { .. }),
        "big catalog dict: {e}"
    );
    // The optimizer's copy of 7,000 inherited resource entries (about 0.8 MB) in each of 600 pages.
    let inherited = inherited_resources_pdf(7_000, 600);
    for max_alloc in [16u64 << 20, 256 << 20] {
        check(
            "inherited resources",
            "read_validate_and_optimize",
            op_read_validate_and_optimize,
            &inherited,
            max_alloc,
        );
    }
    // 8 pages that share an array of 100,000 annotation references (6.4 MB of parse estimate):
    // each page keeps a record and a slot of the cleaned array per entry, about 6 MB a page.
    check(
        "shared annots",
        "read_and_validate",
        op_read_and_validate,
        &shared_annots_pdf(100_000, 8),
        32 << 20,
    );
    // The resource names of a page's content (about 300,000, 15 MB of sets) and the copy of the
    // content behind a glued operator, at 16 MiB.
    check(
        "content names",
        "read_validate_and_optimize",
        op_read_validate_and_optimize,
        &content_names_pdf(300_000),
        16 << 20,
    );
    // The optimizer's per-page record of the visited forms: 1,000 forms in each of 1,500 pages.
    check(
        "shared forms",
        "read_validate_and_optimize",
        op_read_validate_and_optimize,
        &shared_forms_pdf(1_000, 1_500),
        8 << 20,
    );
    // Two documents on one shared ledger of `max` bytes: each alone fits, both do not, so the
    // second read fails and the heap of both stays within `max`.
    let doc = huge_array_pdf(200_000);
    let cancel = AtomicBool::new(false);
    let alone = api::read(&doc, &Config::default(), &cancel).unwrap();
    let (peak, held) = (alone.xref.mem().peak(), alone.xref.mem().used());
    drop(alone);
    assert!(held > 0);
    let max = peak + held / 2;
    let mem = Mem::with_max(max);
    let ((first, second), heap) = measure(|| {
        let first = api::read_with_mem(&doc, &Config::default(), &mem, &cancel);
        let second = api::read_with_mem(&doc, &Config::default(), &mem, &cancel);
        (first, second)
    });
    assert!(first.is_ok(), "shared ledger: the first read fails");
    let e = second.unwrap_err();
    assert!(
        matches!(e.root(), Error::Alloc { .. }),
        "shared ledger: {e}"
    );
    assert!(
        heap <= max + SLACK,
        "shared ledger at {max}: heap peak {heap} is over max + {SLACK}"
    );
    drop(first);
    assert_eq!(mem.used(), 0);
}
