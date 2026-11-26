//! The public entry points (docs/design.md section 3; Go: pkg/api). WP 9 added `read` (api.go:81),
//! WP 22 `read_and_validate` and `validate`, WP 26 `read_validate_and_optimize`, `properties` and
//! `write_context`, and re-exports the `pdfcpu` entry points that byblos calls.

use crate::config::{Cmd, Config};
use crate::env::WriteEnv;
use crate::error::Error;
use crate::ledger::Mem;
use crate::model::Context;
use crate::write::WriteContext;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

pub use crate::doc::create::{
    create_context, create_xref_table_with_root_dict, create_xref_table_with_root_dict_with_mem,
};
pub use crate::doc::property::properties_add;
pub use crate::image::{Image, color_space_components, extract_image};

/// `api.ReadContext`: parses `data` into a [`Context`] holding the cross-reference table, with
/// every object loaded. `cfg` is the document's configuration for its whole life
/// (`ctx.xref.conf`); `cfg.limits` bounds the work. The cancel flag interrupts the read.
///
/// The page count is not known after a read: call [`crate::model::XRefTable::ensure_page_count`]
/// before [`crate::model::XRefTable::page_dict`].
///
/// # Errors
/// [`crate::read::err_empty_input`] for no input, the header, cross-reference, object and
/// encryption errors ([`Error::WrongPassword`]), the limit errors of [`crate::Limits`],
/// [`Error::Cancelled`].
pub fn read(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Result<Context, Error> {
    read_with_mem(data, cfg, &Mem::new(&cfg.limits), cancel)
}

/// As [`read`], charging every allocation of the document to the caller's ledger `mem` instead
/// of a fresh one: several documents of one call share one peak, `mem`'s maximum, and
/// `cfg.limits.max_alloc` is not used (docs/limits.md, "Shared ledger").
///
/// # Errors
/// As [`read`]; [`Error::Alloc`] when `mem` has too little left, also because of what other
/// documents on `mem` hold.
pub fn read_with_mem(
    data: &[u8],
    cfg: &Config,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Context, Error> {
    // `pdfcpu.Read` checks the cancel flag before it does anything else.
    crate::read::read_with_mem(data, cfg, mem, cancel)
}

/// `readAndValidate` (api.go:236): reads and validates, with the read and validation errors
/// wrapped as Go wraps them. The object number of a validation error is not kept (design
/// section 5).
fn read_and_validate_context(
    data: &[u8],
    cfg: &Config,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Context, Error> {
    let mut ctx = read_with_mem(data, cfg, mem, cancel).map_err(|e| e.context("read context"))?;
    // `ValidateContext` checks the cancel flag first.
    crate::validate::xref_table(&mut ctx, cancel).map_err(|e| e.context("validation error"))?;
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    Ok(ctx)
}

/// `api.ReadAndValidate` (api.go:93, without the file I/O): reads `data` and validates it
/// (relaxed), returning the validated [`Context`]. `cfg` is the document's configuration; Go
/// uses `NewDefaultConfiguration`, which is [`Config::default`]. `cfg.limits` bounds the work and
/// the cancel flag interrupts it.
///
/// # Errors
/// The errors of [`read`] with the context "read context", the validation errors with the
/// context "validation error", [`Error::Cancelled`].
pub fn read_and_validate(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Result<Context, Error> {
    read_and_validate_with_mem(data, cfg, &Mem::new(&cfg.limits), cancel)
}

/// As [`read_and_validate`], charging the caller's ledger `mem` (see [`read_with_mem`]).
///
/// # Errors
/// As [`read_and_validate`]; [`Error::Alloc`] when `mem` has too little left.
pub fn read_and_validate_with_mem(
    data: &[u8],
    cfg: &Config,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Context, Error> {
    read_and_validate_context(data, cfg, mem, cancel)
}

/// `api.OptimizeContext` (api.go:145): optimizes `ctx`.
fn optimize_context(ctx: &mut Context, cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    crate::optimize::optimize_xref_table(ctx, cancel).map_err(|e| e.context("optimize context"))
}

/// `api.Validate` (validate.go:118): reads `data` with the command set to validate, validates it
/// (relaxed) and, as `conf.Optimize` is set (the default), optimizes it (validate.go:89-96), so
/// this can fail inside optimize.
///
/// # Errors
/// As [`read_and_validate`], and the optimize errors with the context "optimize context".
pub fn validate(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Result<(), Error> {
    validate_with_mem(data, cfg, &Mem::new(&cfg.limits), cancel)
}

/// As [`validate`], charging the caller's ledger `mem` (see [`read_with_mem`]).
///
/// # Errors
/// As [`validate`]; [`Error::Alloc`] when `mem` has too little left.
pub fn validate_with_mem(
    data: &[u8],
    cfg: &Config,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let cfg = Config {
        cmd: Cmd::Validate,
        ..*cfg
    };
    let mut ctx = read_and_validate_context(data, &cfg, mem, cancel)?;
    optimize_context(&mut ctx, cancel)
}

/// `api.ReadValidateAndOptimize` (api.go:304): reads, validates and optimizes `data`, then caches
/// the form fonts. `cfg.cmd` is the command (it selects what optimize does); `cfg.limits` bounds
/// the work and the cancel flag interrupts it.
///
/// # Errors
/// The errors of [`read_and_validate`] with the context "prepare PDF context", the optimize
/// errors with the context "optimize context", the form font errors with the context "cache form
/// fonts", [`Error::Cancelled`].
pub fn read_validate_and_optimize(
    data: &[u8],
    cfg: &Config,
    cancel: &AtomicBool,
) -> Result<Context, Error> {
    read_validate_and_optimize_with_mem(data, cfg, &Mem::new(&cfg.limits), cancel)
}

/// As [`read_validate_and_optimize`], charging the caller's ledger `mem` (see
/// [`read_with_mem`]).
///
/// # Errors
/// As [`read_validate_and_optimize`]; [`Error::Alloc`] when `mem` has too little left.
pub fn read_validate_and_optimize_with_mem(
    data: &[u8],
    cfg: &Config,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<Context, Error> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let mut ctx = read_and_validate_context(data, cfg, mem, cancel)
        .map_err(|e| e.context("prepare PDF context"))?;

    // The optimize step is optional in Go (`conf.Optimize`), which is always set here.
    optimize_context(&mut ctx, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }

    // TODO in Go: move to form related commands.
    crate::optimize::cache_form_fonts(&mut ctx).map_err(|e| e.context("cache form fonts"))?;
    Ok(ctx)
}

/// `api.Properties` (property.go:33): the properties of the document's information dictionary.
/// The command is forced to list properties (it selects what optimize does), and the document is
/// read, validated and optimized as for [`read_validate_and_optimize`].
///
/// # Errors
/// As [`read_validate_and_optimize`], with the context "list properties" in front, and
/// [`Error::Cancelled`].
pub fn properties(
    data: &[u8],
    cfg: &Config,
    cancel: &AtomicBool,
) -> Result<BTreeMap<String, String>, Error> {
    properties_with_mem(data, cfg, &Mem::new(&cfg.limits), cancel)
}

/// As [`properties`], charging the caller's ledger `mem` (see [`read_with_mem`]).
///
/// # Errors
/// As [`properties`]; [`Error::Alloc`] when `mem` has too little left.
pub fn properties_with_mem(
    data: &[u8],
    cfg: &Config,
    mem: &Mem,
    cancel: &AtomicBool,
) -> Result<BTreeMap<String, String>, Error> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let cfg = Config {
        cmd: Cmd::ListProperties,
        ..*cfg
    };
    let mut ctx = read_validate_and_optimize_with_mem(data, &cfg, mem, cancel)
        .map_err(|e| e.context("list properties"))?;
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    Ok(std::mem::take(&mut ctx.xref.properties))
}

/// `api.WriteContext` (api.go:169): writes `ctx` as a whole new PDF file and returns the bytes.
/// `env` is the clock (the dates of the information dictionary and the file identifier) and the
/// random source (the initialisation vectors of an encrypted file). The output is charged to the
/// document's ledger (`cfg.limits.max_alloc`) while it is built.
///
/// The document changes as Go's does: the information dictionary and the file identifier are
/// updated, redundant objects are freed, and the encryption dictionary keeps the strings it was
/// written with if the document body refers to it. Unlike Go, no other object is left with
/// encrypted strings.
///
/// # Errors
/// The errors of [`crate::write::write_context`], [`Error::Cancelled`].
pub fn write_context(
    ctx: &mut Context,
    env: &WriteEnv,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, Error> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let mut w = WriteContext::new(*env, crate::write::hooks(), ctx.xref.mem().clone());
    // The writer prunes the redundant objects it has written; the document keeps the rest.
    w.optimize = ctx.optimize.take();
    // The working-copy reserve of the pass (docs/limits.md).
    let r = ctx.xref.start_reserve().and_then(|reserve| {
        let r = crate::write::write_context(ctx, &mut w, cancel);
        ctx.xref.end_reserve(reserve);
        r
    });
    ctx.optimize = w.optimize.take();
    r?;
    Ok(w.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Sentinel;
    use crate::object::{Rect, Ref};

    /// The PDF of Go's `catalogEndobjPDF(false)`: the page tree root is object 1, the catalog 3.
    fn catalog_pdf() -> Vec<u8> {
        let objects = [
            "<< /Type /Pages /Kids [2 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 1 0 R /MediaBox [0 0 100 100] /Resources <<>> >>",
            "<< /Type /Catalog /Pages 1 0 R >>",
        ];
        let mut f = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            offsets.push(f.len());
            f.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = f.len();
        f.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for o in offsets {
            f.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        f.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 3 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        f
    }

    // Port of the valid case of TestReadCatalogEndobj, and a PageDict on the result.
    #[test]
    fn read_then_page_count_and_page_dict() {
        let cancel = AtomicBool::new(false);
        let mut ctx = read(&catalog_pdf(), &Config::default(), &cancel).unwrap();
        assert_eq!(ctx.xref.page_count, 0);
        ctx.xref.ensure_page_count().unwrap();
        assert_eq!(ctx.xref.page_count, 1);

        let (d, r, attrs) = ctx.xref.page_dict(1, &cancel).unwrap();
        assert_eq!(
            r,
            Ref {
                num: 2,
                generation: 0
            }
        );
        assert_eq!(d.name_entry(b"Type"), Some(b"Page".as_slice()));
        assert_eq!(attrs.media_box, Some(Rect::new(0.0, 0.0, 100.0, 100.0)));
        assert!(attrs.resources.is_some_and(|d| d.is_empty()));

        let err = ctx.xref.page_dict(2, &cancel).unwrap_err();
        assert!(err.is(Sentinel::PageNotFound), "{err}");
    }

    // Port of TestReadContextEmptyInputPreservesPdfcpuSentinel.
    #[test]
    fn read_of_nothing_is_empty_input() {
        let cancel = AtomicBool::new(false);
        let err = read(b"", &Config::default(), &cancel).unwrap_err();
        assert_eq!(err, crate::read::err_empty_input());
    }

    #[test]
    fn read_and_validate_validates_a_catalog() {
        let cancel = AtomicBool::new(false);
        let ctx = read_and_validate(&catalog_pdf(), &Config::default(), &cancel).unwrap();
        assert!(ctx.xref.valid);
        assert!(validate(&catalog_pdf(), &Config::default(), &cancel).is_ok());
    }

    #[test]
    fn read_and_validate_wraps_the_read_error() {
        let cancel = AtomicBool::new(false);
        let err = read_and_validate(b"", &Config::default(), &cancel).unwrap_err();
        assert!(matches!(err, Error::Context(ref m, _) if &**m == "read context"));
        assert_eq!(err.root(), crate::read::err_empty_input().root());
    }

    #[test]
    fn validate_honours_cancellation() {
        let cancelled = AtomicBool::new(true);
        let err = validate(&catalog_pdf(), &Config::default(), &cancelled).unwrap_err();
        assert_eq!(err.root(), &Error::Cancelled);
    }

    #[test]
    fn documents_read_with_one_mem_share_its_ledger() {
        let cancel = AtomicBool::new(false);
        let alone = read(&catalog_pdf(), &Config::default(), &cancel).unwrap();
        let u = alone.xref.mem().used();
        assert!(u > 0);

        let mem = Mem::with_max(u64::MAX);
        let a = read_with_mem(&catalog_pdf(), &Config::default(), &mem, &cancel).unwrap();
        let b =
            read_and_validate_with_mem(&catalog_pdf(), &Config::default(), &mem, &cancel).unwrap();
        assert!(mem.used() >= 2 * u);
        assert_eq!(a.xref.mem().used(), mem.used());
        let both = mem.used();
        drop(a);
        assert_eq!(mem.used(), both - u);
        drop(b);
        assert_eq!(mem.used(), 0);
    }

    #[test]
    fn validate_and_properties_with_mem_charge_the_callers_ledger() {
        let cancel = AtomicBool::new(false);
        let mem = Mem::with_max(u64::MAX);
        validate_with_mem(&catalog_pdf(), &Config::default(), &mem, &cancel).unwrap();
        assert_eq!(mem.used(), 0);
        assert!(mem.peak() > 0);
        let p = properties(&catalog_pdf(), &Config::default(), &cancel).unwrap();
        let q = properties_with_mem(&catalog_pdf(), &Config::default(), &mem, &cancel).unwrap();
        assert_eq!(p, q);
        let tiny = Mem::with_max(1);
        let err =
            validate_with_mem(&catalog_pdf(), &Config::default(), &tiny, &cancel).unwrap_err();
        assert!(matches!(err.root(), Error::Alloc { .. }), "{err:?}");
    }

    #[test]
    fn read_honours_cancellation() {
        let cancelled = AtomicBool::new(true);
        let err = read(&catalog_pdf(), &Config::default(), &cancelled).unwrap_err();
        assert_eq!(err, Error::Cancelled);
    }

    // The writer on a created document, and a read of its output.
    #[test]
    fn write_context_writes_a_created_document() {
        let cancel = AtomicBool::new(false);
        let mut ctx = crate::doc::create::one_page_context();
        let pdf = write_context(&mut ctx, &WriteEnv::parity(), &cancel).unwrap();
        assert!(pdf.starts_with(b"%PDF-1.7\n"));
        assert!(pdf.ends_with(b"%%EOF\n") || pdf.ends_with(b"%%EOF"));
        // A created document has no optimization context, and the writer leaves it so.
        assert!(ctx.optimize.is_none());
        // The information dictionary and the file identifier were made.
        assert!(ctx.xref.info.is_some());
        assert_eq!(ctx.xref.id.as_ref().map(Vec::len), Some(2));

        let mut back = read(&pdf, &Config::default(), &cancel).unwrap();
        back.xref.ensure_page_count().unwrap();
        assert_eq!(back.xref.page_count, 1);
        let info = back.xref.info.unwrap();
        let d = back.xref.dereference_dict_ref(info).unwrap().unwrap();
        assert_eq!(
            d.string_entry(b"Producer"),
            Some(b"pdfcpu v0.16.4".as_slice())
        );
    }

    #[test]
    fn write_context_is_deterministic_under_the_parity_environment() {
        let cancel = AtomicBool::new(false);
        let a = write_context(
            &mut crate::doc::create::one_page_context(),
            &WriteEnv::parity(),
            &cancel,
        )
        .unwrap();
        let b = write_context(
            &mut crate::doc::create::one_page_context(),
            &WriteEnv::parity(),
            &cancel,
        )
        .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn write_context_of_a_read_document_keeps_its_optimization_context() {
        let cancel = AtomicBool::new(false);
        let mut ctx = read(&catalog_pdf(), &Config::default(), &cancel).unwrap();
        assert!(ctx.optimize.is_some());
        let pdf = write_context(&mut ctx, &WriteEnv::parity(), &cancel).unwrap();
        assert!(pdf.starts_with(b"%PDF-1.7\n"));
        assert!(ctx.optimize.is_some());
    }

    #[test]
    fn write_context_honours_cancellation() {
        let cancelled = AtomicBool::new(true);
        let mut ctx = crate::doc::create::one_page_context();
        let err = write_context(&mut ctx, &WriteEnv::parity(), &cancelled).unwrap_err();
        assert_eq!(err, Error::Cancelled);
    }

    #[test]
    fn properties_added_to_a_document_are_listed_after_a_write() {
        let cancel = AtomicBool::new(false);
        let env = WriteEnv::parity();
        let mut ctx = crate::doc::create::one_page_context();
        let props = BTreeMap::from([("Custom".to_string(), "v\u{e4}lue".to_string())]);
        properties_add(&mut ctx, &props, &env, &cancel).unwrap();
        let pdf = write_context(&mut ctx, &env, &cancel).unwrap();

        let got = properties(&pdf, &Config::default(), &cancel).unwrap();
        assert_eq!(got.get("Custom").map(String::as_str), Some("v\u{e4}lue"));
    }

    #[test]
    fn properties_honours_cancellation() {
        let cancelled = AtomicBool::new(true);
        let err = properties(&catalog_pdf(), &Config::default(), &cancelled).unwrap_err();
        assert_eq!(err, Error::Cancelled);
    }

    #[test]
    fn read_validate_and_optimize_wraps_the_read_error() {
        let cancel = AtomicBool::new(false);
        let err = read_validate_and_optimize(b"", &Config::default(), &cancel).unwrap_err();
        assert!(matches!(err, Error::Context(ref m, _) if &**m == "prepare PDF context"));
        assert_eq!(err.root(), crate::read::err_empty_input().root());
    }
}
