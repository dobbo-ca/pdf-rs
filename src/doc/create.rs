//! A new document (Go: pkg/pdfcpu/createContext.go `CreateXRefTableWithRootDict`,
//! `CreateContext`). `CreateContextWithXRefTable` and `addPageTreeWithoutPage` are not ported:
//! byblos builds its page tree itself.

use crate::config::{Cmd, Config, Limits};
use crate::error::Error;
use crate::ledger::Mem;
use crate::model::{Context, Entry, Version, XRefTable};
use crate::object::{Dict, Object};

/// `CreateXRefTableWithRootDict`: a cross-reference table of version 1.7 that holds the free
/// list head (object 0) and a catalog (object 1, only `/Type /Catalog`).
///
/// `limits` bound the document; the table gets a ledger of `limits.max_alloc` bytes.
/// [`create_context`] sets the configuration.
///
/// # Errors
/// As `XRefTable::ind_ref_for_new_object`; none in practice for a fresh table.
pub fn create_xref_table_with_root_dict(limits: &Limits) -> Result<XRefTable, Error> {
    create_xref_table_with_root_dict_with_mem(limits, &Mem::new(limits))
}

/// As [`create_xref_table_with_root_dict`], charging the caller's ledger `mem` (shared with
/// other documents of one call) instead of a fresh one; `limits.max_alloc` is then not used.
///
/// # Errors
/// As [`create_xref_table_with_root_dict`]; [`Error::Alloc`] when `mem` has too little left.
pub fn create_xref_table_with_root_dict_with_mem(
    limits: &Limits,
    mem: &Mem,
) -> Result<XRefTable, Error> {
    let mut x = XRefTable::new_with_mem(
        Config {
            limits: *limits,
            cmd: Cmd::default(),
        },
        mem.clone(),
    );
    x.table.insert(0, Entry::free_head());
    x.size = Some(1);
    x.header_version = Some(Version::V17);

    let mut root = Dict::default();
    root.insert_name(b"Type", b"Catalog");
    let ir = x.ind_ref_for_new_object(Object::Dict(root))?;
    x.root = Some(ir);
    Ok(x)
}

/// `CreateContext`: a context for `xref` with the configuration `cfg`, which becomes the
/// document's configuration (`ctx.xref.conf`). There is no read context and no optimization
/// context, as in Go.
///
/// The table's ledger keeps the size it was made with; make the table with the `max_alloc` of
/// `cfg.limits`, or with the caller's shared `Mem`
/// ([`create_xref_table_with_root_dict_with_mem`]).
pub fn create_context(mut xref: XRefTable, cfg: Config) -> Context {
    xref.conf = cfg;
    Context {
        xref,
        read: None,
        writing_pages: false,
        dest: false,
        writing_annotation: false,
        optimize: None,
    }
}

/// A context with a catalog, a page tree and one 100 by 100 page, for the tests of the packages
/// that write or change a created document.
#[cfg(test)]
pub(crate) fn one_page_context() -> Context {
    use crate::object::Ref;

    let mut x = create_xref_table_with_root_dict(&Limits::default()).unwrap();
    let mut pages = Dict::default();
    pages.insert_name(b"Type", b"Pages");
    pages.insert_int(b"Count", 1);
    pages.set(b"Kids", Object::Array(Vec::new()));
    let pages_ref = x.ind_ref_for_new_object(Object::Dict(pages)).unwrap();

    let mut page = Dict::default();
    page.insert_name(b"Type", b"Page");
    page.set(b"Parent", Object::Ref(pages_ref));
    page.set(
        b"MediaBox",
        Object::Array(vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(100),
            Object::Integer(100),
        ]),
    );
    page.set(b"Resources", Object::Dict(Dict::default()));
    let page_ref: Ref = x.ind_ref_for_new_object(Object::Dict(page)).unwrap();

    x.dereference_dict_mut(pages_ref)
        .unwrap()
        .unwrap()
        .set(b"Kids", Object::Array(vec![Object::Ref(page_ref)]));
    x.catalog_mut()
        .unwrap()
        .set(b"Pages", Object::Ref(pages_ref));
    create_context(x, Config::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Ref;

    #[test]
    fn the_new_table_has_a_catalog() {
        let mut x = create_xref_table_with_root_dict(&Limits::default()).unwrap();
        assert_eq!(x.size, Some(2));
        assert_eq!(x.header_version, Some(Version::V17));
        assert_eq!(
            x.root,
            Some(Ref {
                num: 1,
                generation: 0
            })
        );
        let head = x.find(0).unwrap();
        assert!(head.free && head.offset == Some(0));
        let root = x.catalog().unwrap();
        assert_eq!(root.name_entry(b"Type"), Some(b"Catalog".as_slice()));
        assert_eq!(root.len(), 1);
        assert_eq!(x.version_string().unwrap(), "1.7");
    }

    #[test]
    fn the_limits_are_the_documents_limits() {
        let limits = Limits {
            max_alloc: 1 << 20,
            max_threads: 1,
            ..Limits::default()
        };
        let x = create_xref_table_with_root_dict(&limits).unwrap();
        assert_eq!(x.conf.limits, limits);
        assert_eq!(x.mem().used(), 0);
    }

    #[test]
    fn create_context_sets_the_configuration() {
        let x = create_xref_table_with_root_dict(&Limits::default()).unwrap();
        let cfg = Config {
            limits: Limits {
                max_decode_bytes: 1234,
                ..Limits::default()
            },
            cmd: Cmd::AddProperties,
        };
        let ctx = create_context(x, cfg);
        assert_eq!(ctx.xref.conf, cfg);
        assert!(ctx.read.is_none() && ctx.optimize.is_none());
        assert_eq!(ctx.read_file_size(), 0);
        assert!(!ctx.writing_pages && !ctx.dest && !ctx.writing_annotation);
    }

    #[test]
    fn the_test_document_has_one_page() {
        let mut ctx = one_page_context();
        ctx.xref.ensure_page_count().unwrap();
        assert_eq!(ctx.xref.page_count, 1);
    }
}
