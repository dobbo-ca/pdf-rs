//! Validation stores the `/Subtype /Image` it infers for a page thumbnail, as pdf-go does
//! (validate/page.go:401 validates the stream map the table holds). T6.pdf has 11 pages, each
//! with its own thumbnail image stream without `/Subtype`.

use pdf::object::Object;
use pdf::{Config, api};
use std::sync::atomic::AtomicBool;

#[test]
fn every_thumbnail_of_t6_gets_subtype_image() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/T6.pdf");
    let data = std::fs::read(path).unwrap();
    let cancel = AtomicBool::new(false);
    let mut ctx = api::read_validate_and_optimize(&data, &Config::default(), &cancel).unwrap();
    ctx.xref.ensure_page_count().unwrap();
    assert_eq!(ctx.xref.page_count, 11);
    for n in 1..=ctx.xref.page_count {
        let (d, _, _) = ctx.xref.page_dict(n, &cancel).unwrap();
        let thumb = d.find(b"Thumb").cloned().expect("a thumbnail");
        let Object::Stream(sd) = ctx.xref.dereference(&thumb).unwrap() else {
            panic!("page {n}: the thumbnail is no stream");
        };
        assert_eq!(
            sd.dict.name_entry(b"Subtype"),
            Some(b"Image".as_slice()),
            "page {n}"
        );
    }
}
