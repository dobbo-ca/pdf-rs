//! What the writer needs for PDF 2.0 and for the document information (Go:
//! pkg/pdfcpu/writeFont.go `prepareCoreFontsForWriting`, and the wiring of the hooks of
//! [`WriteHooks`]).
//!
//! `checkAnnotationAppearancesForWriting` is only called by Go's `WriteIncrement`, which the
//! port does not reach. The annotation-aware parts of the object writer (`writeAnnotationAwareEntry`,
//! `checkWrittenAnnotation`, `writingAnnotationEntry`) are in `objects.rs`.

use super::corefont::ensure_core_font_metrics;
use super::{WriteHooks, check};
use crate::doc::info::ensure_info_dict;
use crate::error::Error;
use crate::model::{Context, Entry, EntryObject, XRefTable};
use crate::object::Object;
use crate::validate::annotation::check_annotation_appearance;
use std::sync::atomic::AtomicBool;

/// The hooks of the real writer: `ensureInfoDict`, `prepareCoreFontsForWriting` and
/// `CheckAnnotationAppearance`. Pass them to `WriteContext::new`.
pub fn hooks() -> WriteHooks {
    WriteHooks {
        ensure_info_dict,
        prepare_core_fonts,
        check_annotation_appearance,
    }
}

/// `prepareCoreFontDict`: completes the metrics of `d` if it is a font dictionary of a standard
/// font. `changed` is set if it changed.
fn prepare_core_font_dict(
    x: &mut XRefTable,
    d: &mut crate::object::Dict,
    changed: &mut bool,
) -> Result<(), Error> {
    let t = d.0.get(b"Type".as_slice()).cloned().unwrap_or(Object::Null);
    if !matches!(x.dereference(&t)?, Object::Name(n) if n == b"Font") {
        return Ok(());
    }
    if ensure_core_font_metrics(x, d)?.is_some() {
        *changed = true;
    }
    Ok(())
}

/// `prepareCoreFontObject`: walks the direct objects of `o` (Go does not follow references: each
/// table entry is visited on its own).
fn prepare_core_font_object(
    x: &mut XRefTable,
    o: &mut Object,
    depth: u64,
    changed: &mut bool,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    x.check_recursion_depth("prepare core font metrics", depth)?;
    let d = match o {
        Object::Dict(d) => d,
        Object::Array(a) => {
            for v in a.iter_mut() {
                prepare_core_font_object(x, v, depth + 1, changed, cancel)?;
            }
            return Ok(());
        }
        // A stream dictionary is walked at the same depth.
        Object::Stream(sd) => &mut sd.dict,
        _ => return Ok(()),
    };
    prepare_core_font_dict(x, d, changed)?;
    for (_, v) in d.iter_mut() {
        prepare_core_font_object(x, v, depth + 1, changed, cancel)?;
    }
    Ok(())
}

/// `prepareCoreFontsForWriting`: for a PDF 2.0 file, adds the explicit metrics that the standard
/// fonts need (9.6.2.1, Table 109) to every font dictionary of the table.
///
/// Objects are visited in object number order; the descriptors it adds are not visited. Go
/// changes the dictionaries in place; this works on a copy of each object and stores it back
/// when it changed, so a reference from a font to its own object sees the old dictionary.
///
/// # Errors
/// [`Error::Cancelled`], the errors of the decode of a lazy object, and the errors of
/// `ensure_core_font_metrics` with the context "prepare PDF 2.0 fonts: object N".
pub fn prepare_core_fonts(ctx: &mut Context, cancel: &AtomicBool) -> Result<(), Error> {
    // See 9.6.2.1, Table 109: imported standard fonts also need explicit metrics in PDF 2.0.
    if !ctx.xref.pdf20()? {
        return Ok(());
    }
    let _keys = ctx
        .xref
        .mem()
        .charge((ctx.xref.table.len() as u64).saturating_mul(8))?;
    let mut obj_nrs = Vec::new();
    obj_nrs.try_reserve_exact(ctx.xref.table.len())?;
    obj_nrs.extend(ctx.xref.table.keys().copied());
    for obj_nr in obj_nrs {
        let lazy = match ctx.xref.table.get(&obj_nr) {
            Some(Entry {
                free: false,
                object: Some(EntryObject::Lazy(_)),
                ..
            }) => true,
            Some(Entry {
                free: false,
                object: Some(_),
                ..
            }) => false,
            _ => continue,
        };
        if lazy {
            ctx.xref.decode_lazy_entry(obj_nr, cancel)?;
        }
        // The working copy is charged before it is made.
        let (_copy, mut o) = match ctx.xref.table.get(&obj_nr) {
            Some(Entry {
                object: Some(EntryObject::Obj(o)),
                ..
            }) if matches!(o, Object::Dict(_) | Object::Array(_) | Object::Stream(_)) => {
                (ctx.xref.mem().charge(o.copy_bytes())?, o.clone())
            }
            _ => continue,
        };
        let mut changed = false;
        let r = prepare_core_font_object(&mut ctx.xref, &mut o, 0, &mut changed, cancel);
        if changed
            && let Some(Entry {
                object: Some(EntryObject::Obj(slot)),
                ..
            }) = ctx.xref.table.get_mut(&obj_nr)
        {
            *slot = o;
        }
        r.map_err(|e| e.context(format!("prepare PDF 2.0 fonts: object {obj_nr}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::Version;
    use crate::object::{Dict, Ref};

    fn doc() -> Context {
        let mut ctx = Context::new(0, Config::default()).unwrap();
        ctx.xref.table.insert(0, Entry::free_head());
        ctx.xref.size = Some(1);
        ctx.xref.header_version = Some(Version::V20);
        ctx
    }

    fn font(base: &str) -> Dict {
        let mut d = Dict::default();
        d.set(b"Type", Object::name("Font"));
        d.set(b"Subtype", Object::name("Type1"));
        d.set(b"BaseFont", Object::name(base));
        d
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn a_standard_font_gets_metrics_in_pdf_20() {
        let mut ctx = doc();
        let f = ctx
            .xref
            .ind_ref_for_new_object(Object::Dict(font("Courier")))
            .unwrap();
        prepare_core_fonts(&mut ctx, &no_cancel()).unwrap();

        let d = ctx.xref.dereference_dict_ref(f).unwrap().unwrap().clone();
        assert_eq!(d.int_entry(b"LastChar"), Some(255));
        let fd = d.indirect_ref_entry(b"FontDescriptor").unwrap();
        assert_eq!(
            fd,
            Ref {
                num: 2,
                generation: 0
            }
        );
        assert!(ctx.xref.dereference_dict_ref(fd).unwrap().is_some());
    }

    #[test]
    fn a_font_inside_a_resource_dictionary_and_a_stream_is_found() {
        let mut ctx = doc();
        let mut fonts = Dict::default();
        fonts.set(b"F1", Object::Dict(font("Helvetica")));
        let mut res = Dict::default();
        res.set(b"Font", Object::Dict(fonts));
        let r = ctx
            .xref
            .ind_ref_for_new_object(Object::Array(vec![Object::Dict(res)]))
            .unwrap();
        let sd = crate::object::StreamDict {
            dict: font("Times-Roman"),
            ..crate::object::StreamDict::default()
        };
        let s = ctx
            .xref
            .ind_ref_for_new_object(Object::Stream(Box::new(sd)))
            .unwrap();

        prepare_core_fonts(&mut ctx, &no_cancel()).unwrap();

        let Object::Array(a) = ctx.xref.dereference(&Object::Ref(r)).unwrap().clone() else {
            panic!("not an array");
        };
        let Object::Dict(res) = &a[0] else { panic!() };
        let Some(Object::Dict(fonts)) = res.find(b"Font") else {
            panic!()
        };
        let Some(Object::Dict(f1)) = fonts.find(b"F1") else {
            panic!()
        };
        assert!(f1.has_entry(b"Widths"));
        let Object::Stream(sd) = ctx.xref.dereference(&Object::Ref(s)).unwrap().clone() else {
            panic!("not a stream");
        };
        assert!(sd.dict.has_entry(b"FontDescriptor"));
    }

    #[test]
    fn nothing_happens_before_pdf_20() {
        let mut ctx = doc();
        ctx.xref.header_version = Some(Version::V17);
        let f = ctx
            .xref
            .ind_ref_for_new_object(Object::Dict(font("Courier")))
            .unwrap();
        prepare_core_fonts(&mut ctx, &no_cancel()).unwrap();
        let d = ctx.xref.dereference_dict_ref(f).unwrap().unwrap().clone();
        assert!(!d.has_entry(b"Widths"));
        assert_eq!(ctx.xref.size, Some(2));
    }

    #[test]
    fn an_error_names_the_object() {
        let mut ctx = doc();
        let mut d = font("Helvetica");
        d.set(b"FirstChar", Object::Integer(32));
        let f = ctx.xref.ind_ref_for_new_object(Object::Dict(d)).unwrap();
        let err = prepare_core_fonts(&mut ctx, &no_cancel()).unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("prepare PDF 2.0 fonts: object {}", f.num)),
            "{err}"
        );
    }

    #[test]
    fn it_honours_cancellation_and_the_recursion_limit() {
        let mut ctx = doc();
        ctx.xref
            .ind_ref_for_new_object(Object::Dict(font("Courier")))
            .unwrap();
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            prepare_core_fonts(&mut ctx, &cancelled).unwrap_err().root(),
            &Error::Cancelled
        );

        let mut ctx = doc();
        let mut o = Object::Array(Vec::new());
        for _ in 0..200 {
            o = Object::Array(vec![o]);
        }
        ctx.xref.ind_ref_for_new_object(o).unwrap();
        let err = prepare_core_fonts(&mut ctx, &no_cancel()).unwrap_err();
        assert!(err.is(crate::error::Sentinel::MaxRecursionDepth), "{err}");
    }
}
