//! Tests of the writer: the ports of the Go tests of write.go that the port reaches
//! (pkg/pdfcpu/write_test.go), and tests of the parts that Go only tests through its API.

use super::*;
use crate::config::Config;
use crate::error::Sentinel;
use crate::object::Ref;
use std::sync::atomic::AtomicUsize;

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn hooks() -> WriteHooks {
    WriteHooks {
        ensure_info_dict: |_, _| Ok(()),
        prepare_core_fonts: |_, _| Ok(()),
        check_annotation_appearance: |_, _| Ok(()),
    }
}

fn r(n: i64) -> Ref {
    Ref {
        num: n,
        generation: 0,
    }
}

fn rf(n: i64) -> Object {
    Object::Ref(r(n))
}

fn dict(entries: Vec<(&str, Object)>) -> Dict {
    let mut d = Dict::default();
    for (k, v) in entries {
        d.set(k.as_bytes(), v);
    }
    d
}

fn put(ctx: &mut Context, n: i64, o: Object) {
    ctx.xref.table.insert(n, Entry::gen0(o));
}

fn media_box() -> Object {
    Object::Array(vec![
        Object::Integer(0),
        Object::Integer(0),
        Object::Integer(100),
        Object::Integer(100),
    ])
}

fn page(parent: i64) -> Object {
    Object::Dict(dict(vec![
        ("Type", Object::name("Page")),
        ("Parent", rf(parent)),
        ("MediaBox", media_box()),
    ]))
}

fn pages_node(parent: Option<i64>, kids: &[i64], count: i64) -> Object {
    let mut d = dict(vec![
        ("Type", Object::name("Pages")),
        ("Kids", Object::Array(kids.iter().map(|&k| rf(k)).collect())),
        ("Count", Object::Integer(count)),
    ]);
    if let Some(p) = parent {
        d.set(b"Parent", rf(p));
    }
    Object::Dict(d)
}

fn stream(mem: &Mem, body: &[u8]) -> Object {
    let n = i64::try_from(body.len()).unwrap();
    let mut d = Dict::default();
    d.insert_int(b"Length", n);
    let mut sd = StreamDict::new(d, 0, Some(n), None, Vec::new());
    sd.raw = Some(mem.bytes_from(body, body.len()).unwrap());
    Object::Stream(Box::new(sd))
}

/// A document of five objects: the catalog (1), the page tree root (2), one page (3) with a
/// content stream (4), and the info dictionary (5).
fn base_doc() -> Context {
    let mut ctx = Context::new(0, Config::default()).unwrap();
    let mem = ctx.xref.mem().clone();
    ctx.xref.header_version = Some(Version::V17);
    ctx.xref.table.insert(0, Entry::free_head());
    put(
        &mut ctx,
        1,
        Object::Dict(dict(vec![
            ("Type", Object::name("Catalog")),
            ("Pages", rf(2)),
        ])),
    );
    put(&mut ctx, 2, pages_node(None, &[3], 1));
    let mut p = page(2);
    if let Object::Dict(d) = &mut p {
        d.set(b"Contents", rf(4));
    }
    put(&mut ctx, 3, p);
    put(&mut ctx, 4, stream(&mem, b"q Q"));
    put(
        &mut ctx,
        5,
        Object::Dict(dict(vec![("Producer", Object::string("test"))])),
    );
    ctx.xref.root = Some(r(1));
    ctx.xref.info = Some(r(5));
    ctx.xref.size = Some(6);
    ctx
}

fn writer(ctx: &Context) -> WriteContext {
    WriteContext::new(WriteEnv::parity(), hooks(), ctx.xref.mem().clone())
}

/// Writes `ctx` with the xref table and no object streams.
fn write_plain(ctx: &mut Context) -> Vec<u8> {
    let mut w = writer(ctx);
    w.write_object_stream = false;
    w.write_xref_stream = false;
    write_context(ctx, &mut w, &no_cancel()).unwrap();
    w.into_bytes()
}

/// Writes `ctx` with object streams and an xref stream.
fn write_compressed(ctx: &mut Context) -> Vec<u8> {
    let mut w = writer(ctx);
    write_context(ctx, &mut w, &no_cancel()).unwrap();
    w.into_bytes()
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

/// The offset that the last `startxref` of `pdf` gives.
fn startxref(pdf: &[u8]) -> usize {
    let marker = b"startxref\n";
    let i = rfind(pdf, marker).expect("missing startxref");
    let line = &pdf[i + marker.len()..];
    let j = line
        .iter()
        .position(|&b| b == b'\n')
        .expect("unterminated startxref offset");
    let offset: usize = std::str::from_utf8(&line[..j]).unwrap().parse().unwrap();
    assert!(offset < pdf.len(), "startxref {offset} outside the file");
    offset
}

/// The numbers `<obj> <gen>` of the `obj` header that starts at `offset`.
fn obj_header_at(pdf: &[u8], offset: usize) -> (i64, i64) {
    let end = pdf[offset..].iter().position(|&b| b == b'\n').unwrap();
    let line = std::str::from_utf8(&pdf[offset..offset + end]).unwrap();
    let mut it = line.split(' ');
    let n = it.next().unwrap().parse().unwrap();
    let g = it.next().unwrap().parse().unwrap();
    assert_eq!(it.next(), Some("obj"), "{line:?}");
    (n, g)
}

// ---------------------------------------------------------------------------------------------
// Ports of the Go tests

// Port of TestWriteContextReturnsCancellation.
#[test]
fn write_context_returns_cancellation() {
    let mut ctx = base_doc();
    let mut w = writer(&ctx);
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        write_context(&mut ctx, &mut w, &cancelled),
        Err(Error::Cancelled)
    );
    assert!(w.bytes().is_empty());
}

// Port of TestWriteXRefSubsectionReturnsCancellationBeforeEntryProcessing.
#[test]
fn write_xref_subsection_returns_cancellation() {
    let ctx = base_doc();
    let mut w = writer(&ctx);
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        write_xref_subsection(&ctx, &mut w, 0, 1, &cancelled),
        Err(Error::Cancelled)
    );
}

// Port of TestCreateXRefStreamReturnsCancellationBeforeEntryProcessing.
#[test]
fn create_xref_stream_returns_cancellation() {
    let ctx = base_doc();
    let w = writer(&ctx);
    let cancelled = AtomicBool::new(true);
    let err = create_xref_stream(&ctx, &w, [1, 1, 1], &[0], &cancelled).unwrap_err();
    assert_eq!(err, Error::Cancelled);
}

// Port of TestWriteDeepObjectReturnsCancellationBeforeObjectProcessing.
#[test]
fn write_deep_object_returns_cancellation() {
    let mut ctx = base_doc();
    let mut w = writer(&ctx);
    let cancelled = AtomicBool::new(true);
    let err = objects::write_deep_object(&mut ctx, &mut w, rf(0), &cancelled).unwrap_err();
    assert_eq!(err, Error::Cancelled);
}

// Port of TestWriteXRefSection (through write_context and read).
#[test]
fn writes_an_xref_section() {
    let mut ctx = base_doc();
    let pdf = write_plain(&mut ctx);

    assert!(pdf.starts_with(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n"));
    assert!(pdf.ends_with(b"%%EOF\n"));
    let offset = startxref(&pdf);
    assert!(pdf[offset..].starts_with(b"xref\n"), "startxref {offset}");

    // The table: one subsection of six entries, whose offsets are the objects.
    let table = &pdf[offset + 5..];
    assert!(table.starts_with(b"0 6\n"));
    let lines = &table[4..];
    assert_eq!(&lines[..20], b"0000000000 65535 f \n");
    for n in 1..=5usize {
        let line = std::str::from_utf8(&lines[20 * n..20 * (n + 1)]).unwrap();
        assert!(line.ends_with(" 00000 n \n"), "{line:?}");
        let off: usize = line[..10].parse().unwrap();
        assert_eq!(obj_header_at(&pdf, off), (n as i64, 0), "object {n}");
    }
    assert_eq!(count(&pdf, b"/Type/ObjStm"), 0);
    assert_eq!(count(&pdf, b"/ID[<"), 1);

    // And it reads back as a file with an xref section and one page.
    let cancel = no_cancel();
    let mut back = crate::read::read(&pdf, &Config::default(), &cancel).unwrap();
    assert_eq!(
        back.read.as_ref().map(|r| r.using_xref_streams),
        Some(false)
    );
    back.xref.ensure_page_count().unwrap();
    assert_eq!(back.xref.page_count, 1);
}

// Port of TestWriteXRefStreamIncludesSelfEntry.
#[test]
fn xref_stream_includes_its_own_entry() {
    let mut ctx = base_doc();
    let pdf = write_compressed(&mut ctx);

    let offset = startxref(&pdf);
    let (obj_nr, gen_nr) = obj_header_at(&pdf, offset);
    assert_eq!(gen_nr, 0);

    let cancel = no_cancel();
    let mut back = crate::read::read(&pdf, &Config::default(), &cancel).unwrap();
    let info = back.read.clone().unwrap();
    assert!(info.using_xref_streams);
    assert!(info.using_object_streams);
    let entry = back
        .xref
        .table
        .get(&obj_nr)
        .unwrap_or_else(|| panic!("xref stream object {obj_nr} has no entry of its own"));
    assert_eq!(entry.offset, Some(i64::try_from(offset).unwrap()));

    // The page tree is in the object stream, and it still reads.
    back.xref.ensure_page_count().unwrap();
    assert_eq!(back.xref.page_count, 1);
    assert_eq!(count(&pdf, b"/Type/ObjStm"), 1);
}

// ---------------------------------------------------------------------------------------------
// The page tree

#[test]
fn writes_nested_page_trees() {
    let mut ctx = base_doc();
    put(&mut ctx, 2, pages_node(None, &[6, 3], 2));
    put(&mut ctx, 6, pages_node(Some(2), &[7], 1));
    put(&mut ctx, 7, page(6));
    ctx.xref.size = Some(8);
    let pdf = write_plain(&mut ctx);

    let cancel = no_cancel();
    let mut back = crate::read::read(&pdf, &Config::default(), &cancel).unwrap();
    back.xref.ensure_page_count().unwrap();
    assert_eq!(back.xref.page_count, 2);
    // The written counts are the pages written below each node.
    let node = back.xref.dereference_dict_ref(r(6)).unwrap().unwrap();
    assert_eq!(node.int_entry(b"Count"), Some(1));
    let root = back.xref.dereference_dict_ref(r(2)).unwrap().unwrap();
    assert_eq!(root.int_entry(b"Count"), Some(2));
}

#[test]
fn page_tree_cycles_and_duplicates_are_rejected() {
    // A node that is its own descendant.
    let mut ctx = base_doc();
    put(&mut ctx, 6, pages_node(Some(2), &[2], 1));
    put(&mut ctx, 2, pages_node(None, &[6], 1));
    ctx.xref.size = Some(7);
    let mut w = writer(&ctx);
    let err = write_context(&mut ctx, &mut w, &no_cancel()).unwrap_err();
    assert!(err.is(Sentinel::PageTreeCycle), "{err}");

    // A node with two parents.
    let mut ctx = base_doc();
    put(&mut ctx, 2, pages_node(None, &[6, 6], 2));
    put(&mut ctx, 6, pages_node(Some(2), &[3], 1));
    ctx.xref.size = Some(7);
    let mut w = writer(&ctx);
    let err = write_context(&mut ctx, &mut w, &no_cancel()).unwrap_err();
    assert!(err.is(Sentinel::PageTreeDuplicate), "{err}");
}

#[test]
fn page_tree_errors_are_the_ones_of_go() {
    let cases: [(&str, Object, &str); 4] = [
        (
            "NoParent",
            Object::Dict(dict(vec![("Type", Object::name("Page"))])),
            "missing parent",
        ),
        (
            "BadType",
            Object::Dict(dict(vec![("Type", Object::name("Other"))])),
            "unexpected dict type: Other",
        ),
        (
            "NoType",
            Object::Dict(Dict::default()),
            "missing page node dict type",
        ),
        ("NotARef", Object::Integer(1), "missing indirect reference"),
    ];
    for (name, kid, want) in cases {
        let mut ctx = base_doc();
        if matches!(kid, Object::Integer(_)) {
            let mut d = dict(vec![
                ("Type", Object::name("Pages")),
                ("Count", Object::Integer(1)),
            ]);
            d.set(b"Kids", Object::Array(vec![kid]));
            put(&mut ctx, 2, Object::Dict(d));
        } else {
            put(&mut ctx, 3, kid);
        }
        let mut w = writer(&ctx);
        let err = write_context(&mut ctx, &mut w, &no_cancel()).unwrap_err();
        assert!(err.to_string().contains(want), "{name}: {err}");
    }
}

#[test]
fn object_streams_hold_at_most_100_objects() {
    let mut ctx = base_doc();
    let kids: Vec<i64> = (10..160).collect();
    put(&mut ctx, 2, pages_node(None, &kids, 150));
    for &k in &kids {
        put(&mut ctx, k, page(2));
    }
    ctx.xref.size = Some(160);
    let pdf = write_compressed(&mut ctx);

    // 150 pages and the page tree root: two streams, of 100 and 51 objects.
    assert_eq!(count(&pdf, b"/Type/ObjStm"), 2);
    assert!(count(&pdf, b"/N 100") >= 1);
    assert!(count(&pdf, b"/N 51") >= 1);

    let cancel = no_cancel();
    let mut back = crate::read::read(&pdf, &Config::default(), &cancel).unwrap();
    back.xref.ensure_page_count().unwrap();
    assert_eq!(back.xref.page_count, 150);
}

// ---------------------------------------------------------------------------------------------
// PDF 2.0

static ANNOT_CHECKS: AtomicUsize = AtomicUsize::new(0);
static FONT_PREPARATIONS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn pdf20_calls_the_hooks() {
    let mut ctx = base_doc();
    ctx.xref.header_version = Some(Version::V20);
    put(
        &mut ctx,
        6,
        Object::Dict(dict(vec![
            ("Type", Object::name("Annot")),
            ("Subtype", Object::name("Text")),
            ("P", rf(3)),
        ])),
    );
    let mut p = page(2);
    if let Object::Dict(d) = &mut p {
        d.set(b"Contents", rf(4));
        d.set(b"Annots", Object::Array(vec![rf(6)]));
    }
    put(&mut ctx, 3, p);
    ctx.xref.size = Some(7);

    let mut w = writer(&ctx);
    w.hooks = WriteHooks {
        ensure_info_dict: |_, _| Err(Error::Corrupt("no info dict for PDF 2.0".into())),
        prepare_core_fonts: |_, _| {
            FONT_PREPARATIONS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        check_annotation_appearance: |_, d| {
            assert_eq!(d.name_entry(b"Subtype"), Some(b"Text".as_slice()));
            ANNOT_CHECKS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    };
    write_context(&mut ctx, &mut w, &no_cancel()).unwrap();
    let pdf = w.into_bytes();

    assert!(pdf.starts_with(b"%PDF-2.0\n"));
    assert_eq!(FONT_PREPARATIONS.load(Ordering::SeqCst), 1);
    assert_eq!(ANNOT_CHECKS.load(Ordering::SeqCst), 1);
    // The annotation was written, and the annotation context is back where it was.
    assert!(!ctx.writing_annotation && !ctx.dest && !ctx.writing_pages);
    assert!(ctx.xref.table.get(&6).is_some_and(|e| e.compressed));
}

// ---------------------------------------------------------------------------------------------
// The cross-reference section

#[test]
fn int64_to_buf_cases() {
    assert_eq!(int64_to_buf(0, 1), [0]);
    assert_eq!(int64_to_buf(2, 1), [2]);
    assert_eq!(int64_to_buf(258, 3), [0, 1, 2]);
    assert_eq!(int64_to_buf(0xFFFF, 2), [0xFF, 0xFF]);
    // A value that does not fit is not cut.
    assert_eq!(int64_to_buf(65536, 2), [1, 0, 0]);
    // Go loops while the value is positive.
    assert_eq!(int64_to_buf(-5, 2), [0, 0]);
    assert_eq!(byte_len(0), 0);
    assert_eq!(byte_len(255), 1);
    assert_eq!(byte_len(256), 2);
}

#[test]
fn xref_stream_content_and_subsections() {
    let mut ctx = Context::new(0, Config::default()).unwrap();
    ctx.xref.table.insert(0, Entry::free_head());
    put(&mut ctx, 1, Object::Null);
    put(&mut ctx, 2, Object::Null);
    put(&mut ctx, 5, Object::Null);
    let mut compressed = Entry::gen0(Object::Null);
    compressed.compressed = true;
    compressed.object_stream = Some(9);
    compressed.object_stream_ind = Some(4);
    ctx.xref.table.insert(6, compressed);
    let mut w = writer(&ctx);
    w.table.extend([(1, 9), (2, 20), (5, 30), (6, 6)]);

    let (content, index) =
        create_xref_stream(&ctx, &w, [1, 2, 2], &[0, 1, 2, 5, 6], &no_cancel()).unwrap();
    let want: &[u8] = &[
        0, 0, 0, 0xFF, 0xFF, // 0: free, next free 0, generation 65535
        1, 0, 9, 0, 0, // 1: offset 9
        1, 0, 20, 0, 0, // 2: offset 20
        1, 0, 30, 0, 0, // 5: offset 30
        2, 0, 9, 0, 4, // 6: object stream 9, index 4
    ];
    assert_eq!(&content[..], want);
    // The subsections: 0..=2, then 5..=6.
    let index: Vec<i64> = index
        .iter()
        .map(|o| match o {
            Object::Integer(i) => *i,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(index, [0, 3, 5, 2]);

    // An object that was not written has no offset to give.
    w.table.remove(&5);
    let err = create_xref_stream(&ctx, &w, [1, 2, 2], &[0, 1, 2, 5], &no_cancel()).unwrap_err();
    // Go names the loop index, 3, and not the object, 5.
    assert_eq!(
        err,
        Error::Corrupt("missing write offset for obj #3".into())
    );
}

#[test]
fn xref_table_needs_an_entry() {
    let ctx = Context::new(0, Config::default()).unwrap();
    let mut w = writer(&ctx);
    let err = write_xref_table(&ctx, &mut w, &no_cancel()).unwrap_err();
    assert_eq!(err.class(), "panic");
}

#[test]
fn xref_table_splits_at_gaps_and_refuses_compressed_entries() {
    let mut ctx = Context::new(0, Config::default()).unwrap();
    ctx.xref.size = Some(8);
    ctx.xref.root = Some(r(1));
    ctx.xref.table.insert(0, Entry::free_head());
    put(&mut ctx, 1, Object::Null);
    put(&mut ctx, 4, Object::Null);
    put(&mut ctx, 5, Object::Null);
    let mut w = writer(&ctx);
    w.table.extend([(1, 15), (4, 40), (5, 50)]);
    write_xref_table(&ctx, &mut w, &no_cancel()).unwrap();
    let text = String::from_utf8(w.bytes().to_vec()).unwrap();
    assert!(text.starts_with("xref\n0 2\n0000000000 65535 f \n0000000015 00000 n \n4 2\n"));
    assert!(text.contains("0000000040 00000 n \n0000000050 00000 n \ntrailer\n"));
    assert!(text.contains("/Root 1 0 R/Size 8"));
    assert!(text.ends_with("startxref\n0\n"));

    ctx.xref.table.get_mut(&4).unwrap().compressed = true;
    let mut w = writer(&ctx);
    w.table.extend([(1, 15), (4, 40), (5, 50)]);
    let err = write_xref_table(&ctx, &mut w, &no_cancel()).unwrap_err();
    assert_eq!(err, Error::Corrupt("compressed entries present".into()));
}

// ---------------------------------------------------------------------------------------------
// Free list and redundant objects

fn free(next: i64, generation: i64) -> Entry {
    Entry {
        free: true,
        offset: Some(next),
        generation: Some(generation),
        ..Entry::default()
    }
}

#[test]
fn undelete_takes_an_object_off_the_free_list() {
    let mut ctx = Context::new(0, Config::default()).unwrap();
    let x = &mut ctx.xref;
    x.table.insert(0, Entry::free_head());
    x.table.get_mut(&0).unwrap().offset = Some(3);
    x.table.insert(3, free(5, 2));
    x.table.insert(5, free(0, 1));

    undelete_object(x, 3).unwrap();
    assert_eq!(x.table[&0].offset, Some(5));
    let e = &x.table[&3];
    assert!(!e.free && e.offset.is_none());
    assert_eq!(e.generation, Some(1));

    // An object that is not on the list stays as it is.
    undelete_object(x, 9).unwrap();
    assert_eq!(x.table[&0].offset, Some(5));

    // The last object of the list.
    undelete_object(x, 5).unwrap();
    assert_eq!(x.table[&0].offset, Some(0));
    assert_eq!(x.table[&5].generation, Some(0));
}

#[test]
fn undelete_gives_up_on_a_circular_free_list() {
    let mut ctx = Context::new(0, Config::default()).unwrap();
    let x = &mut ctx.xref;
    x.table.insert(0, Entry::free_head());
    x.table.get_mut(&0).unwrap().offset = Some(3);
    x.table.insert(3, free(3, 0));
    let err = undelete_object(x, 9).unwrap_err();
    assert_eq!(err, Error::Corrupt("free list is circular".into()));
}

#[test]
fn a_null_object_is_written_and_undeleted() {
    let mut ctx = base_doc();
    // The catalog's /Names refers to the free object 6, which is on the free list.
    ctx.xref.catalog_mut().unwrap().set(b"Names", rf(6));
    ctx.xref.table.insert(6, free(0, 0));
    ctx.xref.table.get_mut(&0).unwrap().offset = Some(6);
    ctx.xref.size = Some(7);
    let pdf = write_plain(&mut ctx);

    assert_eq!(count(&pdf, b"6 0 obj\nnull\nendobj\n"), 1);
    let e = &ctx.xref.table[&6];
    assert!(!e.free);
    assert_eq!(ctx.xref.table[&0].offset, Some(0));
}

#[test]
fn redundant_objects_that_were_not_written_are_freed() {
    let mut ctx = base_doc();
    let mut w = writer(&ctx);
    w.table.extend([(1, 15), (2, 30), (3, 45), (4, 60)]);
    w.optimize = Some(RedundantObjs {
        duplicate_font_objs: BTreeSet::from([2]),
        duplicate_image_objs: BTreeSet::new(),
        duplicate_info_objs: BTreeSet::from([5]),
    });

    delete_redundant_objects(&mut ctx, &mut w, &no_cancel()).unwrap();

    // 5 is a duplicate info object that was not written. 2 was written, so it is not freed,
    // and it is no longer a candidate.
    let e = &ctx.xref.table[&5];
    assert!(e.free && e.generation == Some(1) && e.object.is_none());
    assert_eq!(ctx.xref.table[&0].offset, Some(5));
    assert!(!ctx.xref.table[&2].free);
    let opt = w.optimize.as_ref().unwrap();
    assert!(opt.duplicate_font_objs.is_empty());
    assert!(opt.duplicate_info_objs.contains(&5));

    // The object streams of the source are freed too, if they were not written.
    let mut ctx = base_doc();
    ctx.read.as_mut().unwrap().object_streams.insert(4);
    let mut w = writer(&ctx);
    w.table.extend([(1, 15), (2, 30), (3, 45), (5, 70)]);
    w.optimize = Some(RedundantObjs::default());
    delete_redundant_objects(&mut ctx, &mut w, &no_cancel()).unwrap();
    assert!(ctx.xref.table[&4].free);

    // Without an optimization context nothing is freed.
    let mut ctx = base_doc();
    let mut w = writer(&ctx);
    delete_redundant_objects(&mut ctx, &mut w, &no_cancel()).unwrap();
    assert!(!ctx.xref.table[&4].free);
}

// ---------------------------------------------------------------------------------------------
// Signature dictionaries

fn sig_entries() -> Vec<(&'static str, Object)> {
    vec![
        ("Type", Object::name("Sig")),
        (
            "ByteRange",
            Object::Array(vec![
                Object::Integer(0),
                Object::Integer(1),
                Object::Integer(2),
                Object::Integer(3),
            ]),
        ),
        ("Contents", Object::Hex(b"ab".to_vec())),
        ("Filter", Object::name("Adobe")),
        ("SubFilter", Object::name("sub")),
        ("M", Object::string("D:2000")),
        ("Extra", Object::Integer(7)),
    ]
}

#[test]
fn sig_dict_detection() {
    assert!(objects::sig_dict(&dict(sig_entries())));
    assert!(objects::sig_dict(&dict(vec![
        ("Contents", Object::Null),
        ("ByteRange", Object::Null),
    ])));
    assert!(!objects::sig_dict(&dict(vec![(
        "Type",
        Object::name("Page")
    )])));
    assert!(!objects::sig_dict(&dict(vec![("Contents", Object::Null)])));
}

#[test]
fn sig_dict_has_a_fixed_layout() {
    let got = objects::sig_dict_pdf_string(&dict(sig_entries())).unwrap();
    let want = format!(
        "<</ByteRange[0 1 2 3]{}/Contents<ab>/Type/Sig/Filter/Adobe/SubFilter/sub/M(D:2000)>>",
        " ".repeat(27)
    );
    assert_eq!(String::from_utf8(got).unwrap(), want);

    // A time stamp has no signer entries.
    let mut e = sig_entries();
    e.retain(|(k, _)| *k != "SubFilter");
    e.push(("SubFilter", Object::name("ETSI.RFC3161")));
    let got = objects::sig_dict_pdf_string(&dict(e)).unwrap();
    assert!(!String::from_utf8(got).unwrap().contains("/M("));

    // Values must be direct, and the fixed entries must be there.
    let mut e = sig_entries();
    e.retain(|(k, _)| *k != "Filter");
    e.push(("Filter", rf(9)));
    let err = objects::sig_dict_pdf_string(&dict(e)).unwrap_err();
    assert_eq!(
        err,
        Error::Corrupt("signature dict entry Filter: value must be direct".into())
    );
    let mut e = sig_entries();
    e.retain(|(k, _)| *k != "ByteRange");
    let err = objects::sig_dict_pdf_string(&dict(e)).unwrap_err();
    assert_eq!(err.class(), "panic");
}

// ---------------------------------------------------------------------------------------------
// The output buffer

#[test]
fn the_output_is_charged_to_the_ledger() {
    let mem = Mem::with_max(8);
    let mut w = WriteContext::new(WriteEnv::parity(), hooks(), mem.clone());
    assert_eq!(w.eol, "\n");
    assert_eq!(w.write(b"abcd").unwrap(), 4);
    assert_eq!(mem.used(), 4);
    w.write_eol().unwrap();
    assert_eq!(w.bytes(), b"abcd\n");
    assert!(mem.used() >= 4);

    // Over the ledger: an error, and nothing is added.
    let err = w.write(b"0123456789").unwrap_err();
    assert!(matches!(err, Error::Alloc { .. }), "{err:?}");
    assert_eq!(w.bytes(), b"abcd\n");

    let bytes = w.into_bytes();
    assert_eq!(bytes, b"abcd\n");
    assert_eq!(mem.used(), 0);
}

#[test]
fn offsets_are_kept_by_hand() {
    let ctx = base_doc();
    let mut w = writer(&ctx);
    assert!(!w.has_write_offset(7));
    w.advance(10);
    w.set_write_offset(7);
    assert!(w.has_write_offset(7));
    assert_eq!(w.table[&7], 10);
    w.advance(usize::MAX);
    assert_eq!(w.offset, i64::MAX);
}

// Go's repaired RootDict is the catalog map of the table: the root version that write.go:149
// deletes from it is not written.
#[test]
fn the_root_version_of_a_repaired_catalog_is_not_written() {
    let mut ctx = base_doc();
    let catalog = dict(vec![
        ("Type", Object::name("Catalog")),
        ("Pages", rf(2)),
        ("Version", Object::name("1.7")),
    ]);
    put(&mut ctx, 1, Object::Dict(catalog.clone()));
    ctx.xref.root_dict = Some(catalog);
    ctx.xref.root_version = Some(Version::V17);
    assert_eq!(count(&write_plain(&mut ctx), b"/Version"), 0);
}

// Go's writeDictObject encrypts the table's map in place: when the body reaches the encryption
// dictionary, writeEncryptDict writes it again with the strings encrypted, as the first copy is.
#[test]
fn a_reachable_encryption_dictionary_is_written_encrypted_twice() {
    let mut ctx = base_doc();
    put(
        &mut ctx,
        1,
        Object::Dict(dict(vec![
            ("Type", Object::name("Catalog")),
            ("Pages", rf(2)),
            ("X", rf(6)),
        ])),
    );
    put(
        &mut ctx,
        6,
        Object::Dict(dict(vec![
            ("Filter", Object::name("Standard")),
            ("V", Object::Integer(1)),
            ("R", Object::Integer(2)),
            ("O", Object::string("owner")),
            ("U", Object::string("user")),
            ("P", Object::Integer(-4)),
        ])),
    );
    ctx.xref.size = Some(7);
    ctx.xref.encrypt = Some(r(6));
    ctx.xref.e = Some(crate::model::Enc {
        v: 1,
        r: 2,
        ..Default::default()
    });
    ctx.xref.enc_key = vec![1, 2, 3, 4, 5];

    let pdf = write_plain(&mut ctx);
    let bodies: Vec<&[u8]> = pdf
        .windows(8)
        .enumerate()
        .filter(|(_, w)| *w == b"6 0 obj\n")
        .map(|(i, _)| {
            let rest = &pdf[i + 8..];
            &rest[..rest.windows(6).position(|w| w == b"endobj").unwrap()]
        })
        .collect();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(count(bodies[1], b"(owner)"), 0);
    let stored = ctx.xref.encrypt_dict().unwrap();
    assert!(!matches!(stored.find(b"O"), Some(Object::String(s)) if s == b"owner"));
}
