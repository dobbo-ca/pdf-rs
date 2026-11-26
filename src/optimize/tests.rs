//! The unit tests of `optimize.go` that the port reaches (pkg/pdfcpu/optimize_test.go), and
//! tests of the phases on small documents. The tests of the content-stream deduplication are
//! not ported: it is off by configuration.

use super::*;
use crate::config::{Cmd, Config, Limits};
use crate::error::Sentinel;
use crate::model::{Entry, Version};

fn r(n: i64) -> Ref {
    Ref {
        num: n,
        generation: 0,
    }
}

fn rf(n: i64) -> Object {
    Object::Ref(r(n))
}

fn name(s: &str) -> Object {
    Object::name(s)
}

fn dict(entries: &[(&str, Object)]) -> Dict {
    let mut d = Dict::default();
    for (k, v) in entries {
        d.set(k.as_bytes(), v.clone());
    }
    d
}

fn obj(entries: &[(&str, Object)]) -> Object {
    Object::Dict(dict(entries))
}

fn cancel() -> AtomicBool {
    AtomicBool::new(false)
}

/// A stream with `body` as its encoded bytes.
fn stream(x: &XRefTable, entries: &[(&str, Object)], body: &[u8]) -> Object {
    let n = i64::try_from(body.len()).unwrap();
    let mut d = dict(entries);
    d.insert_int(b"Length", n);
    let mut sd = StreamDict::new(d, 0, Some(n), None, Vec::new());
    sd.raw = Some(x.mem().bytes_from(body, body.len()).unwrap());
    Object::Stream(Box::new(sd))
}

fn put(ctx: &mut Context, n: i64, o: Object) {
    ctx.xref.table.insert(n, Entry::gen0(o));
    let size = ctx.xref.size.unwrap_or(0).max(n + 1);
    ctx.xref.size = Some(size);
}

/// A document of a catalog (1) and a page tree root (2) with no pages. The command is the one
/// that does not consolidate the page resources, as the Go tests do with
/// `OptimizeResourceDicts=false`.
fn doc() -> Context {
    let mut ctx = Context::new(0, Config::default()).unwrap();
    ctx.xref.conf.cmd = Cmd::ListProperties;
    ctx.xref.header_version = Some(Version::V17);
    ctx.xref.table.insert(0, Entry::free_head());
    ctx.xref.root = Some(r(1));
    put(
        &mut ctx,
        1,
        obj(&[("Type", name("Catalog")), ("Pages", rf(2))]),
    );
    put(
        &mut ctx,
        2,
        obj(&[
            ("Type", name("Pages")),
            ("Kids", Object::Array(Vec::new())),
            ("Count", Object::Integer(0)),
        ]),
    );
    ctx
}

/// Adds the page `n` with the entries `extra` as the last child of the page tree root.
fn add_page(ctx: &mut Context, n: i64, extra: &[(&str, Object)]) {
    let mut entries = vec![("Type", name("Page")), ("Parent", rf(2))];
    entries.extend(extra.iter().cloned());
    put(ctx, n, obj(&entries));
    ctx.xref.page_count += 1;
    let count = ctx.xref.page_count;
    let d = dict_mut(ctx, 2);
    if let Some(Object::Array(kids)) = d.find_mut(b"Kids") {
        kids.push(rf(n));
    }
    d.set(b"Count", Object::Integer(count));
}

fn dict_mut(ctx: &mut Context, n: i64) -> &mut Dict {
    match ctx.xref.table.get_mut(&n).and_then(|e| e.object.as_mut()) {
        Some(EntryObject::Obj(Object::Dict(d))) => d,
        Some(EntryObject::Obj(Object::Stream(sd))) => &mut sd.dict,
        _ => panic!("object {n} is no dictionary"),
    }
}

fn dict_of(ctx: &Context, n: i64) -> &Dict {
    match ctx.xref.table.get(&n).and_then(|e| e.object.as_ref()) {
        Some(EntryObject::Obj(Object::Dict(d))) => d,
        Some(EntryObject::Obj(Object::Stream(sd))) => &sd.dict,
        _ => panic!("object {n} is no dictionary"),
    }
}

/// The reference that `key` of the resource dictionary `kind` of page `n` holds.
fn resource_ref(ctx: &Context, n: i64, kind: &str, key: &str) -> Option<Ref> {
    dict_of(ctx, n)
        .dict_entry(b"Resources")?
        .dict_entry(kind.as_bytes())?
        .indirect_ref_entry(key.as_bytes())
}

fn run(ctx: &mut Context) -> Result<(), Error> {
    optimize_xref_table(ctx, &cancel())
}

/// `optimizeFontAndImages` with the optimization context kept for the test to look at.
fn walk(ctx: &mut Context) -> (OptimizationContext, Result<(), Error>) {
    let mut o = OptimizationContext::new();
    let c = cancel();
    let result = Optimizer::new(&mut ctx.xref, &mut o, &c).optimize_font_and_images();
    (o, result)
}

fn image(ctx: &Context) -> Object {
    stream(
        &ctx.xref,
        &[
            ("Subtype", name("Image")),
            ("Width", rf(22)),
            ("Height", Object::Integer(4)),
        ],
        b"abcd",
    )
}

#[test]
fn the_cancel_flag_stops_it_before_anything_else() {
    let cancelled = AtomicBool::new(true);
    let mut ctx = doc();
    assert_eq!(
        optimize_xref_table(&mut ctx, &cancelled),
        Err(Error::Cancelled)
    );
    assert!(!ctx.xref.optimized);
}

#[test]
fn a_document_without_pages_is_left_as_it_is() {
    let mut ctx = doc();
    ctx.optimize = Some(RedundantObjs {
        duplicate_info_objs: BTreeSet::from([5]),
        ..RedundantObjs::default()
    });
    run(&mut ctx).unwrap();
    assert!(!ctx.xref.optimized);
    assert!(
        ctx.optimize
            .as_ref()
            .is_some_and(|o| o.duplicate_info_objs.contains(&5))
    );
}

#[test]
fn the_sets_of_the_writer_survive_and_the_document_is_marked_optimized() {
    let mut ctx = doc();
    add_page(&mut ctx, 3, &[]);
    ctx.optimize = Some(RedundantObjs {
        duplicate_info_objs: BTreeSet::from([5]),
        ..RedundantObjs::default()
    });
    run(&mut ctx).unwrap();
    assert!(ctx.xref.optimized);
    let red = ctx.optimize.as_ref().unwrap();
    assert!(red.duplicate_info_objs.contains(&5));
    assert!(red.duplicate_font_objs.is_empty() && red.duplicate_image_objs.is_empty());
}

#[test]
fn the_first_reference_to_a_free_object_becomes_a_reference_to_one_null_object() {
    let mut ctx = doc();
    add_page(&mut ctx, 3, &[]);
    ctx.xref.table.insert(
        9,
        Entry {
            free: true,
            generation: Some(1),
            ..Entry::default()
        },
    );
    ctx.xref.size = Some(10);
    let cat = dict_mut(&mut ctx, 1);
    cat.set(b"Foo", rf(9));
    cat.set(b"Bar", obj(&[("X", rf(9))]));
    cat.set(
        b"Baz",
        Object::Array(vec![
            Object::Integer(1),
            Object::Ref(Ref {
                num: 9,
                generation: 3,
            }),
        ]),
    );
    run(&mut ctx).unwrap();

    // Go marks the object as visited when it meets the reference first, in key order (Bar), so
    // the later references to it are left as they are.
    let cat = dict_of(&ctx, 1);
    assert_eq!(
        cat.dict_entry(b"Bar").unwrap().indirect_ref_entry(b"X"),
        Some(r(10))
    );
    assert_eq!(cat.indirect_ref_entry(b"Foo"), Some(r(9)));
    assert!(matches!(
        cat.array_entry(b"Baz").map(|a| a.as_slice()),
        Some([
            Object::Integer(1),
            Object::Ref(Ref {
                num: 9,
                generation: 3
            })
        ])
    ));
    let null = &ctx.xref.table[&10];
    assert!(matches!(null.object, Some(EntryObject::Obj(Object::Null))));
    assert_eq!(null.ref_count, 1);
    assert_eq!(ctx.xref.size, Some(11));
}

#[test]
fn the_error_of_a_free_reference_names_its_path() {
    let mut ctx = doc();
    add_page(&mut ctx, 3, &[]);
    ctx.xref.table.insert(
        9,
        Entry {
            free: true,
            generation: Some(1),
            ..Entry::default()
        },
    );
    dict_mut(&mut ctx, 1).set(b"Foo", rf(9));
    // No Size: the null object cannot be inserted.
    ctx.xref.size = None;
    let err = run(&mut ctx).unwrap_err();
    assert!(
        err.to_string().contains(
            "fix references to free objects: root dict: Foo: insert null object for free obj#9"
        ),
        "{err}"
    );
}

#[test]
fn a_reference_chain_is_walked_without_recursion() {
    let mut ctx = doc();
    add_page(&mut ctx, 3, &[]);
    // A long chain of dictionaries, each reached through the previous one.
    let n = 5000;
    for i in 0..n {
        put(&mut ctx, 100 + i, obj(&[("Next", rf(101 + i))]));
    }
    put(&mut ctx, 100 + n, obj(&[]));
    dict_mut(&mut ctx, 1).set(b"Chain", rf(100));
    run(&mut ctx).unwrap();
    assert!(ctx.xref.optimized);
}

#[test]
fn the_page_piece_info_and_the_root_piece_info_are_deleted() {
    let mut ctx = doc();
    put(&mut ctx, 30, obj(&[("Private", Object::Integer(1))]));
    put(&mut ctx, 31, obj(&[("Private", Object::Integer(2))]));
    dict_mut(&mut ctx, 1).set(b"PieceInfo", rf(30));
    add_page(&mut ctx, 3, &[("PieceInfo", rf(31))]);
    run(&mut ctx).unwrap();
    assert!(!dict_of(&ctx, 1).has_entry(b"PieceInfo"));
    assert!(!dict_of(&ctx, 3).has_entry(b"PieceInfo"));
    assert!(ctx.xref.table[&30].free && ctx.xref.table[&31].free);
}

#[test]
fn duplicate_fonts_are_replaced_by_the_first() {
    let mut ctx = doc();
    let font = obj(&[
        ("Type", name("Font")),
        ("Subtype", name("Type1")),
        ("BaseFont", name("Helvetica")),
    ]);
    put(&mut ctx, 10, font.clone());
    put(&mut ctx, 11, font);
    let res = |n| obj(&[("Font", obj(&[("F1", rf(n))]))]);
    add_page(&mut ctx, 3, &[("Resources", res(10))]);
    add_page(&mut ctx, 4, &[("Resources", res(11))]);
    run(&mut ctx).unwrap();

    assert_eq!(resource_ref(&ctx, 3, "Font", "F1"), Some(r(10)));
    assert_eq!(resource_ref(&ctx, 4, "Font", "F1"), Some(r(10)));
    assert_eq!(ctx.xref.table[&10].ref_count, 1);
    let red = ctx.optimize.as_ref().unwrap();
    assert!(red.duplicate_font_objs.contains(&11) && !red.duplicate_font_objs.contains(&10));
}

#[test]
fn fonts_that_differ_stay() {
    let mut ctx = doc();
    let font = |base| {
        obj(&[
            ("Type", name("Font")),
            ("Subtype", name("Type1")),
            ("BaseFont", name(base)),
        ])
    };
    put(&mut ctx, 10, font("Helvetica"));
    put(&mut ctx, 11, font("Helvetica"));
    // The same name, but not the same dictionary.
    dict_mut(&mut ctx, 11).set(b"Encoding", name("WinAnsiEncoding"));
    let res = |n| obj(&[("Font", obj(&[("F1", rf(n))]))]);
    add_page(&mut ctx, 3, &[("Resources", res(10))]);
    add_page(&mut ctx, 4, &[("Resources", res(11))]);
    let (o, result) = walk(&mut ctx);
    result.unwrap();
    assert_eq!(resource_ref(&ctx, 4, "Font", "F1"), Some(r(11)));
    assert!(o.duplicate_fonts.is_empty());
    assert_eq!(o.fonts[b"Helvetica".as_slice()], [10, 11]);
    assert_eq!(o.page_fonts[&1], BTreeSet::from([11]));
}

#[test]
fn duplicate_images_are_replaced_and_the_widths_of_the_kept_ones_made_direct() {
    let mut ctx = doc();
    put(&mut ctx, 22, Object::Integer(8));
    let (a, b) = (image(&ctx), image(&ctx));
    put(&mut ctx, 20, a);
    put(&mut ctx, 21, b);
    let res = |n| obj(&[("XObject", obj(&[("Im1", rf(n))]))]);
    add_page(&mut ctx, 3, &[("Resources", res(20))]);
    add_page(&mut ctx, 4, &[("Resources", res(21))]);
    run(&mut ctx).unwrap();

    assert_eq!(resource_ref(&ctx, 4, "XObject", "Im1"), Some(r(20)));
    assert_eq!(dict_of(&ctx, 20).int_entry(b"Width"), Some(8));
    assert_eq!(dict_of(&ctx, 21).indirect_ref_entry(b"Width"), Some(r(22)));
    let red = ctx.optimize.as_ref().unwrap();
    // The duplicate and what only it reaches.
    assert_eq!(red.duplicate_image_objs, BTreeSet::from([21, 22]));
}

#[test]
fn duplicate_forms_are_replaced_and_their_piece_info_deleted() {
    let mut ctx = doc();
    put(&mut ctx, 41, obj(&[]));
    let form = |ctx: &Context| {
        stream(
            &ctx.xref,
            &[("Subtype", name("Form")), ("PieceInfo", rf(41))],
            b"q Q",
        )
    };
    let (a, b) = (form(&ctx), form(&ctx));
    put(&mut ctx, 40, a);
    put(&mut ctx, 42, b);
    let res = |n| obj(&[("XObject", obj(&[("Fm", rf(n))]))]);
    add_page(&mut ctx, 3, &[("Resources", res(40))]);
    add_page(&mut ctx, 4, &[("Resources", res(42))]);
    run(&mut ctx).unwrap();

    assert!(!dict_of(&ctx, 40).has_entry(b"PieceInfo"));
    assert!(ctx.xref.table[&41].free);
    // The forms were equal once their PieceInfo was gone.
    assert_eq!(resource_ref(&ctx, 4, "XObject", "Fm"), Some(r(40)));
    assert_eq!(ctx.xref.table[&40].ref_count, 1);
}

#[test]
fn the_soft_mask_of_a_graphics_state_registers_its_image() {
    let mut ctx = doc();
    put(&mut ctx, 22, Object::Integer(8));
    let a = image(&ctx);
    put(&mut ctx, 20, a);
    put(&mut ctx, 50, obj(&[("SMask", obj(&[("G", rf(20))]))]));
    add_page(
        &mut ctx,
        3,
        &[("Resources", obj(&[("ExtGState", obj(&[("GS1", rf(50))]))]))],
    );
    let (o, result) = walk(&mut ctx);
    result.unwrap();
    assert_eq!(o.page_images[&0], BTreeSet::from([20]));
    // The name is the graphics state's, then `G`.
    assert_eq!(o.image_objects[&20].resource_names[&0], b"GS1.G");
}

#[test]
fn the_resources_of_a_form_are_processed_once_per_page_and_inherit_the_prefix() {
    let mut ctx = doc();
    put(&mut ctx, 22, Object::Integer(8));
    let a = image(&ctx);
    put(&mut ctx, 20, a);
    let form = stream(
        &ctx.xref,
        &[
            ("Subtype", name("Form")),
            ("Resources", obj(&[("XObject", obj(&[("Inner", rf(20))]))])),
        ],
        b"q Q",
    );
    put(&mut ctx, 40, form);
    let res = obj(&[("XObject", obj(&[("Fm", rf(40))]))]);
    add_page(&mut ctx, 3, &[("Resources", res)]);
    let (o, result) = walk(&mut ctx);
    result.unwrap();
    assert_eq!(o.image_objects[&20].resource_names[&0], b"Fm.Inner");
    assert_eq!(o.form_resource_cache[&0], BTreeSet::from([40]));
}

#[test]
fn the_font_of_a_null_entry_is_repaired_with_arial() {
    let mut ctx = doc();
    let arial = obj(&[
        ("Type", name("Font")),
        ("Subtype", name("TrueType")),
        ("BaseFont", name("Arial")),
    ]);
    put(&mut ctx, 10, arial);
    let mut fonts = dict(&[("F1", rf(10))]);
    fonts.set(b"F2", Object::Null);
    add_page(
        &mut ctx,
        3,
        &[("Resources", obj(&[("Font", Object::Dict(fonts))]))],
    );
    let (o, result) = walk(&mut ctx);
    result.unwrap();
    assert_eq!(o.corrupt_font_res_dicts.len(), 1);
    assert_eq!(resource_ref(&ctx, 3, "Font", "F2"), Some(r(10)));
}

// Port of TestOptimizeXRefTableResourceErrorIncludesDeepPageContext.
#[test]
fn a_resource_error_names_the_page_the_resource_and_the_phase() {
    let mut ctx = doc();
    let pattern = stream(&ctx.xref, &[("Subtype", name("Pattern"))], b"");
    put(&mut ctx, 20, pattern);
    add_page(
        &mut ctx,
        3,
        &[("Resources", obj(&[("XObject", obj(&[("Im1", rf(20))]))]))],
    );
    let err = run(&mut ctx).unwrap_err();
    let text = err.to_string();
    for want in [
        "optimize fonts and images",
        "page tree",
        "page 1 obj#",
        "optimize resources",
        "XObject",
        "XObject resource Im1 obj#",
        "dereference XObject",
    ] {
        assert!(text.contains(want), "{want} in {text}");
    }
}

// Port of TestOptimizeXRefTableSkipsUnclassifiableXObject.
#[test]
fn an_xobject_without_a_subtype_is_skipped() {
    let mut ctx = doc();
    let content = stream(&ctx.xref, &[], b"BT (Hello World!) Tj ET");
    put(&mut ctx, 20, content);
    add_page(
        &mut ctx,
        3,
        &[("Resources", obj(&[("XObject", obj(&[("Im0", rf(20))]))]))],
    );
    let (o, result) = walk(&mut ctx);
    result.unwrap();
    assert!(!dict_of(&ctx, 20).has_entry(b"Subtype"));
    assert_eq!(o.page_images.len(), 1);
    assert!(o.page_images.values().all(|s| s.is_empty()));
}

// Port of TestOptimizeSMaskResourcesSkipsUnclassifiableXObject.
#[test]
fn a_soft_mask_without_a_subtype_is_skipped() {
    let mut ctx = doc();
    let content = stream(&ctx.xref, &[], b"");
    put(&mut ctx, 20, content);
    let smask = dict(&[("G", rf(20))]);
    let mut o = OptimizationContext::new();
    let c = cancel();
    let mut op = Optimizer::new(&mut ctx.xref, &mut o, &c);
    let mut vis = Visited::new(op.x.mem());
    op.optimize_smask_resources(&smask, &mut vis, b"", &Loc::indirect(1), 0, 1)
        .unwrap();
    drop(op);
    assert!(o.image_objects.is_empty());
}

// Port of TestFormResourcesVisitedPerPage.
#[test]
fn form_resources_are_visited_once_per_page() {
    let mut ctx = doc();
    let mut o = OptimizationContext::new();
    let c = cancel();
    let mut op = Optimizer::new(&mut ctx.xref, &mut o, &c);
    assert!(!op.form_resources_visited(0, 7).unwrap());
    assert!(op.form_resources_visited(0, 7).unwrap());
    assert!(!op.form_resources_visited(1, 7).unwrap());
}

// Port of TestEnsureDirectWidthForXObjsErrorIncludesImageContext.
#[test]
fn an_unresolvable_width_names_the_image() {
    let mut ctx = doc();
    let img = stream(&ctx.xref, &[("Width", rf(999))], b"");
    put(&mut ctx, 7, img);
    let mut o = OptimizationContext::new();
    o.page_images.insert(0, BTreeSet::from([7]));
    o.image_objects.insert(7, ImageObject::default());
    let c = cancel();
    let mut op = Optimizer::new(&mut ctx.xref, &mut o, &c);
    let err = op.ensure_direct_width_for_xobjs().unwrap_err();
    assert!(
        err.to_string().contains("image obj#7: resolve width"),
        "{err}"
    );
}

// Port of TestHandleDuplicateImageObjectUsesStreamHashes.
#[test]
fn images_are_grouped_by_the_hash_of_their_stream() {
    let mut ctx = doc();
    for (n, raw) in [(1, b"first"), (2, b"other"), (3, b"first")] {
        let s = stream(&ctx.xref, &[], raw);
        put(&mut ctx, n + 100, s);
    }
    let mut o = OptimizationContext::new();
    o.page_slots = 1;
    let c = cancel();
    let sd = |ctx: &Context, n: i64| match &ctx.xref.table[&n].object {
        Some(EntryObject::Obj(Object::Stream(s))) => (**s).clone(),
        _ => panic!("no stream"),
    };
    let (s1, s2, s3) = (sd(&ctx, 101), sd(&ctx, 102), sd(&ctx, 103));
    let mut op = Optimizer::new(&mut ctx.xref, &mut o, &c);
    for (nr, s) in [(101, &s1), (102, &s2)] {
        let (original, already) = op.handle_duplicate_image_object(s, b"Im", nr, 0).unwrap();
        assert_eq!((original, already), (None, false));
        op.o.image_objects.insert(
            nr,
            ImageObject {
                resource_names: BTreeMap::from([(0, b"Im".to_vec())]),
            },
        );
    }
    let (original, already) = op
        .handle_duplicate_image_object(&s3, b"Im3", 103, 0)
        .unwrap();
    assert_eq!((original, already), (Some(101), false));
    drop(op);
    assert_eq!(o.image_object_hashes.len(), 2);
    assert_eq!(o.duplicate_images[&103], 101);
    assert_eq!(o.image_objects[&101].resource_names[&0], b"Im3");
}

// Port of TestDeduplicationPreservesGeneration, for the sites that are reached.
#[test]
fn a_replacement_reference_keeps_the_generation_of_the_original() {
    for generation in [0, 5] {
        for kind in ["form", "font", "image", "font repair"] {
            let mut ctx = doc();
            for (n, g) in [(100, generation), (101, 9)] {
                let s = stream(&ctx.xref, &[], b"q Q");
                let mut e = Entry::gen0(s);
                e.generation = Some(g);
                ctx.xref.table.insert(n, e);
            }
            ctx.xref.table.get_mut(&100).unwrap().ref_count = 1;
            let font = obj(&[
                ("Type", name("Font")),
                ("Subtype", name("Type1")),
                ("BaseFont", name("Helvetica")),
            ]);
            if kind == "font" {
                for n in [100, 101] {
                    ctx.xref.table.get_mut(&n).unwrap().object =
                        Some(EntryObject::Obj(font.clone()));
                }
            }
            let mut resource = dict(&[(
                "R",
                Object::Ref(Ref {
                    num: 101,
                    generation: 9,
                }),
            )]);
            if kind == "font repair" {
                resource.set(b"R", Object::Null);
            }
            put(&mut ctx, 102, Object::Dict(resource));
            let loc = Loc::indirect(102);
            let sd101 = match &ctx.xref.table[&101].object {
                Some(EntryObject::Obj(Object::Stream(s))) => (**s).clone(),
                _ => StreamDict::default(),
            };

            let mut o = OptimizationContext::new();
            o.page_slots = 1;
            let c = cancel();
            let mut op = Optimizer::new(&mut ctx.xref, &mut o, &c);
            match kind {
                "form" => {
                    op.o.form_stream_cache.insert(100);
                    let ir = op.optimize_xobject_form(101).unwrap();
                    assert_eq!(ir.map(|r| r.generation), Some(generation), "{kind}");
                    assert_eq!(ir.map(|r| r.num), Some(100), "{kind}");
                    assert_eq!(op.x.table[&100].ref_count, 2, "{kind}");
                    continue;
                }
                "font" => {
                    op.o.fonts.insert(b"Helvetica".to_vec(), vec![100]);
                    op.o.font_objects.insert(100, FontObject::default());
                    op.optimize_font_resources_dict(&loc, 0, b"").unwrap();
                }
                "image" => {
                    // Go builds this map from the registered images on first use; here it starts
                    // empty and is filled as images are found.
                    op.o.image_object_hashes.insert(sha256(b"q Q"), vec![100]);
                    op.o.image_objects.insert(100, ImageObject::default());
                    op.optimize_xobject_image(&sd101, b"", b"R", &loc, 101, 0)
                        .unwrap();
                }
                _ => {
                    op.o.fonts.insert(b"Arial".to_vec(), vec![100]);
                    op.o.corrupt_font_res_dicts = vec![loc.clone()];
                    op.fix_corrupt_font_res_dicts().unwrap();
                }
            }
            drop(op);
            let want = Some(Ref {
                num: 100,
                generation,
            });
            assert_eq!(dict_of(&ctx, 102).indirect_ref_entry(b"R"), want, "{kind}");
            if kind != "font repair" {
                assert_eq!(ctx.xref.table[&100].ref_count, 2, "{kind}");
            }
        }
    }
}

// Port of TestOptimizeResourceDictsErrorIncludesPageContext.
#[test]
fn a_page_tree_that_does_not_resolve_names_the_page() {
    let mut ctx = doc();
    add_page(&mut ctx, 3, &[]);
    ctx.xref.conf.cmd = Cmd::Validate;
    dict_mut(&mut ctx, 1).set(b"Pages", rf(999));
    let err = run(&mut ctx).unwrap_err();
    assert!(err.to_string().contains("page 1: resource dict"), "{err}");
}

// Port of TestOptimizeResourceDictsConsolidatesInheritedResources.
#[test]
fn inherited_resources_move_to_the_pages_for_the_validate_command() {
    let mut ctx = doc();
    ctx.xref.conf.cmd = Cmd::Validate;
    dict_mut(&mut ctx, 2).set(b"Resources", obj(&[("Font", obj(&[("F0", name("root"))]))]));
    add_page(
        &mut ctx,
        3,
        &[(
            "Resources",
            obj(&[("XObject", obj(&[("X0", name("pageOne"))]))]),
        )],
    );
    add_page(&mut ctx, 4, &[]);
    run(&mut ctx).unwrap();

    let one = dict_of(&ctx, 3).dict_entry(b"Resources").unwrap();
    assert!(one.dict_entry(b"Font").is_some() && one.dict_entry(b"XObject").is_some());
    let two = dict_of(&ctx, 4).dict_entry(b"Resources").unwrap();
    assert!(two.dict_entry(b"Font").is_some());
    assert!(two.dict_entry(b"XObject").is_none());
}

#[test]
fn a_chain_of_forms_deeper_than_the_recursion_limit_is_an_error() {
    let mut ctx = doc();
    ctx.xref.conf.limits = Limits {
        max_recursion_depth: 2,
        ..Limits::default()
    };
    for n in 0..4 {
        let res = if n < 3 {
            obj(&[("XObject", obj(&[("F", rf(11 + n))]))])
        } else {
            obj(&[])
        };
        let body: &[u8] = if n % 2 == 0 { b"q Q" } else { b"Q q q" };
        let form = stream(
            &ctx.xref,
            &[("Subtype", name("Form")), ("Resources", res)],
            body,
        );
        put(&mut ctx, 10 + n, form);
    }
    add_page(
        &mut ctx,
        3,
        &[("Resources", obj(&[("XObject", obj(&[("F", rf(10))]))]))],
    );
    let err = run(&mut ctx).unwrap_err();
    assert!(err.is(Sentinel::MaxRecursionDepth), "{err}");
}

// Port of TestCacheFormFontsErrorIncludesFormFontObjectContext.
#[test]
fn a_form_font_without_a_name_names_the_object() {
    let mut ctx = doc();
    put(
        &mut ctx,
        10,
        obj(&[("Type", name("Font")), ("Subtype", name("Type1"))]),
    );
    ctx.xref.form = Some(dict(&[("DR", obj(&[("Font", obj(&[("F1", rf(10))]))]))]));
    let err = cache_form_fonts(&mut ctx).unwrap_err();
    assert!(
        err.to_string()
            .contains("form font obj#10: parse font name"),
        "{err}"
    );
}

#[test]
fn the_fonts_of_the_form_are_registered() {
    let mut ctx = doc();
    put(
        &mut ctx,
        10,
        obj(&[
            ("Type", name("Font")),
            ("Subtype", name("Type1")),
            ("BaseFont", name("ABCDEF+Helvetica")),
            ("Encoding", name("WinAnsiEncoding")),
        ]),
    );
    put(
        &mut ctx,
        11,
        obj(&[
            ("Type", name("Font")),
            ("Subtype", name("Type1")),
            ("BaseFont", name("Courier")),
        ]),
    );
    ctx.xref.form = Some(dict(&[(
        "DR",
        obj(&[(
            "Font",
            obj(&[("F1", rf(10)), ("F2", rf(11)), ("F3", Object::Integer(3))]),
        )]),
    )]));
    let mut o = OptimizationContext::new();
    cache_form_fonts_in(&mut ctx.xref, &mut o).unwrap();
    assert_eq!(o.fonts[b"Helvetica".as_slice()], [10]);
    assert_eq!(o.form_font_objects.len(), 2);
    assert_eq!(
        o.form_font_objects[&10],
        FontObject {
            resource_names: vec![b"F1".to_vec()],
            prefix: b"ABCDEF".to_vec(),
            font_name: b"Helvetica".to_vec(),
            subtype_name: b"Type1".to_vec(),
            encoding_name: b"WinAnsiEncoding".to_vec(),
        }
    );
    assert_eq!(o.form_font_objects[&11].encoding_name, b"Built-in");

    // The public call does the same for a document, and a document without a form is fine.
    cache_form_fonts(&mut ctx).unwrap();
    ctx.xref.form = None;
    cache_form_fonts(&mut ctx).unwrap();
}

#[test]
fn a_font_object_keeps_each_resource_name_once() {
    let mut f = FontObject::default();
    f.add_resource_name(b"F1");
    f.add_resource_name(b"F2");
    f.add_resource_name(b"F1");
    assert_eq!(f.resource_names, [b"F1".to_vec(), b"F2".to_vec()]);
    let mut i = ImageObject::default();
    i.add_resource_name(0, b"A");
    i.add_resource_name(0, b"B");
    assert_eq!(i.resource_names[&0], b"B");
}

#[test]
fn visited_gives_back_what_a_call_added() {
    let mut v = Visited::new(&crate::ledger::Mem::with_max(u64::MAX));
    v.push(r(1)).unwrap();
    let mark = v.mark();
    v.push(r(2)).unwrap();
    v.push(r(3)).unwrap();
    assert!(v.contains(r(2)) && v.contains(r(3)));
    v.reset(mark);
    assert!(v.contains(r(1)) && !v.contains(r(2)) && !v.contains(r(3)));
    // A reference with another generation is another reference.
    assert!(!v.contains(Ref {
        num: 1,
        generation: 1
    }));
}

#[test]
fn the_page_number_must_have_a_slot() {
    let mut ctx = doc();
    let mut o = OptimizationContext::new();
    let c = cancel();
    let mut op = Optimizer::new(&mut ctx.xref, &mut o, &c);
    assert_eq!(
        op.page_fonts(0).unwrap_err(),
        Error::Panic("index out of range")
    );
    op.o.page_slots = 1;
    let used = op.x.mem().used();
    assert!(op.page_fonts(0).is_ok());
    assert!(op.page_images(1).is_err());
    // The page sets are charged to the document ledger.
    assert_eq!(op.x.mem().used(), used + PAGE_SET_BYTES);
}

#[test]
fn the_sets_of_the_writer_round_trip() {
    let red = RedundantObjs {
        duplicate_font_objs: BTreeSet::from([1]),
        duplicate_image_objs: BTreeSet::from([2]),
        duplicate_info_objs: BTreeSet::from([3]),
    };
    let o = OptimizationContext::from_redundant(red);
    assert!(o.fonts.is_empty() && o.cache.is_empty());
    let back = o.into_redundant();
    assert_eq!(back.duplicate_font_objs, BTreeSet::from([1]));
    assert_eq!(back.duplicate_image_objs, BTreeSet::from([2]));
    assert_eq!(back.duplicate_info_objs, BTreeSet::from([3]));
}
