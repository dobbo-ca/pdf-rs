//! Adding properties to the document information dictionary (Go: pkg/pdfcpu/property.go
//! `PropertiesAdd`, `preparePropertiesInfo`). `PropertiesRemove` is not ported: byblos never
//! calls it.

use super::info::ensure_info_dict;
use crate::env::WriteEnv;
use crate::error::Error;
use crate::model::{Context, Version};
use crate::object::Object;
use crate::object::text::escaped_utf16_string;
use crate::write::{WriteContext, check, ensure_file_id, hooks};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

/// `preparePropertiesInfo`: makes sure there is an information dictionary (before PDF 2.0; a
/// PDF 2.0 file must have one already) and a file identifier.
fn prepare_properties_info(
    ctx: &mut Context,
    env: &WriteEnv,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    if ctx.xref.version()? < Version::V20 {
        ensure_info_dict(ctx, env).map_err(|e| e.context("Info dictionary: ensure"))?;
    }
    if ctx.xref.info.is_none() {
        return Err(Error::Corrupt("Info dictionary: missing".into()));
    }
    // `ensureFileID` only reads the clock of the writer.
    let w = WriteContext::new(*env, hooks(), ctx.xref.mem().clone());
    ensure_file_id(ctx, &w).map_err(|e| e.context("file ID: ensure"))?;
    check(cancel)
}

/// `PropertiesAdd`: sets `props` in the information dictionary, creating it and the file
/// identifier if need be. `env` is the clock of the dates and the file identifier.
///
/// A property is stored as a UTF-16BE text string. The values go into the table's `properties`
/// as the text of that string (lossily: Go keeps the escaped bytes in a Go string).
///
/// Properties are applied in key order. An error leaves the ones before it applied, as Go does.
///
/// # Errors
/// [`Error::Cancelled`], [`Error::Corrupt`] for a missing information dictionary or an invalid
/// file identifier, the errors of the dereference of the information dictionary
/// ([`crate::Sentinel::ExpectedDict`] if it is not a dictionary).
pub fn properties_add(
    ctx: &mut Context,
    props: &BTreeMap<String, String>,
    env: &WriteEnv,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    check(cancel)?;
    prepare_properties_info(ctx, env, cancel)?;
    let info = ctx
        .xref
        .info
        .ok_or_else(|| Error::Corrupt("Info dictionary: missing".into()))?;
    let missing = || Error::Corrupt("Info dictionary: missing object".into());
    if ctx
        .xref
        .dereference_dict_ref(info)
        .map_err(|e| e.context("Info dictionary: dereference"))?
        .is_none()
    {
        return Err(missing());
    }

    for (k, v) in props {
        check(cancel)?;
        let s = escaped_utf16_string(v.as_bytes())
            .map_err(|e| e.context(format!("Info dictionary property {k:?}: encode value")))?;
        let text = String::from_utf8_lossy(&s).into_owned();
        let d = ctx.xref.dereference_dict_mut(info)?.ok_or_else(missing)?;
        d.set(k.as_bytes(), Object::String(s));
        ctx.xref.properties.insert(k.clone(), text);
    }

    check(cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::create::one_page_context;
    use crate::error::Sentinel;
    use crate::model::Entry;

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn props(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn pdf20(ctx: &mut Context) {
        ctx.xref.header_version = Some(Version::V20);
        ctx.xref.root_version = None;
        ctx.xref.info = None;
    }

    // Port of TestPropertiesAddReportsInfoDictionaryDereference.
    #[test]
    fn reports_the_info_dictionary_dereference() {
        let mut ctx = one_page_context();
        pdf20(&mut ctx);
        ctx.xref.info = Some(ctx.xref.ind_ref_for_new_object(Object::Integer(1)).unwrap());
        let err = properties_add(
            &mut ctx,
            &props(&[("name", "value")]),
            &WriteEnv::parity(),
            &no_cancel(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("Info dictionary: dereference"),
            "{err}"
        );
        assert!(err.is(Sentinel::ExpectedDict), "{err}");
    }

    // Port of TestPropertiesAddReportsMissingInfoDictionary.
    #[test]
    fn reports_a_missing_info_dictionary() {
        let mut ctx = one_page_context();
        pdf20(&mut ctx);
        let err = properties_add(
            &mut ctx,
            &props(&[("name", "value")]),
            &WriteEnv::parity(),
            &no_cancel(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("Info dictionary: missing"),
            "{err}"
        );
    }

    // Port of TestPreparePropertiesInfoReportsDistinctPhases.
    #[test]
    fn prepare_reports_distinct_phases() {
        let env = WriteEnv::parity();

        let mut ctx = one_page_context();
        ctx.xref.info = Some(ctx.xref.ind_ref_for_new_object(Object::Integer(1)).unwrap());
        let err = prepare_properties_info(&mut ctx, &env, &no_cancel()).unwrap_err();
        assert!(err.to_string().contains("Info dictionary: ensure"), "{err}");
        assert!(err.is(Sentinel::ExpectedDict), "{err}");

        let mut ctx = one_page_context();
        pdf20(&mut ctx);
        let err = prepare_properties_info(&mut ctx, &env, &no_cancel()).unwrap_err();
        assert_eq!(
            err,
            Error::Corrupt("Info dictionary: missing".into()),
            "no cause and no context"
        );

        let mut ctx = one_page_context();
        ctx.xref.id = Some(vec![Object::Hex(b"ONE".to_vec())]);
        let err = prepare_properties_info(&mut ctx, &env, &no_cancel()).unwrap_err();
        assert!(err.to_string().contains("file ID: ensure"), "{err}");
        assert!(
            err.to_string()
                .contains("id must be an array with 2 elements"),
            "{err}"
        );
    }

    #[test]
    fn adds_properties_to_a_new_info_dictionary() {
        let mut ctx = one_page_context();
        properties_add(
            &mut ctx,
            &props(&[("alpha", "one"), ("beta", "t\u{e4}o")]),
            &WriteEnv::parity(),
            &no_cancel(),
        )
        .unwrap();

        let info = ctx.xref.info.unwrap();
        let d = ctx
            .xref
            .dereference_dict_ref(info)
            .unwrap()
            .unwrap()
            .clone();
        // UTF-16BE with a byte order mark.
        assert_eq!(
            d.string_entry(b"alpha"),
            Some(b"\xfe\xff\0o\0n\0e".as_slice())
        );
        assert_eq!(
            d.string_entry(b"Producer"),
            Some(b"pdfcpu v0.16.4".as_slice())
        );
        assert!(ctx.xref.properties.contains_key("alpha"));
        assert!(ctx.xref.properties.contains_key("beta"));
        // The file identifier is there, twice the same.
        let id = ctx.xref.id.as_ref().unwrap();
        assert_eq!(id.len(), 2);
    }

    #[test]
    fn replaces_a_property_and_keeps_the_others() {
        let mut ctx = one_page_context();
        let env = WriteEnv::parity();
        properties_add(
            &mut ctx,
            &props(&[("a", "1"), ("b", "2")]),
            &env,
            &no_cancel(),
        )
        .unwrap();
        properties_add(&mut ctx, &props(&[("a", "3")]), &env, &no_cancel()).unwrap();
        let info = ctx.xref.info.unwrap();
        let d = ctx
            .xref
            .dereference_dict_ref(info)
            .unwrap()
            .unwrap()
            .clone();
        assert_eq!(d.string_entry(b"a"), Some(b"\xfe\xff\x003".as_slice()));
        assert_eq!(d.string_entry(b"b"), Some(b"\xfe\xff\x002".as_slice()));
    }

    #[test]
    fn honours_cancellation() {
        let mut ctx = one_page_context();
        let cancelled = AtomicBool::new(true);
        let err = properties_add(
            &mut ctx,
            &props(&[("a", "1")]),
            &WriteEnv::parity(),
            &cancelled,
        )
        .unwrap_err();
        assert_eq!(err, Error::Cancelled);
        assert!(ctx.xref.info.is_none());
    }

    #[test]
    fn an_info_entry_that_is_free_is_reported_missing() {
        let mut ctx = one_page_context();
        pdf20(&mut ctx);
        let r = ctx.xref.ind_ref_for_new_object(Object::Null).unwrap();
        ctx.xref.info = Some(r);
        if let Some(e) = ctx.xref.entry_mut(r.num) {
            *e = Entry {
                free: true,
                ..Entry::default()
            };
        }
        let err = properties_add(
            &mut ctx,
            &props(&[("a", "1")]),
            &WriteEnv::parity(),
            &no_cancel(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("Info dictionary: missing object"),
            "{err}"
        );
    }

    // Go's `ID` shares its array with the xref stream it was read from, so the new file
    // identifier shows in that stream's dictionary too.
    #[test]
    fn updates_the_id_of_the_xref_stream_it_was_read_from() {
        let mut ctx = one_page_context();
        let id = vec![Object::Hex(b"AA".to_vec()), Object::Hex(b"BB".to_vec())];
        let mut x = crate::object::XRefStreamDict::default();
        x.stream.dict.set(b"ID", Object::Array(id.clone()));
        let num = ctx.xref.size.unwrap();
        ctx.xref.size = Some(num + 1);
        ctx.xref.table.insert(
            num,
            Entry {
                object: Some(crate::model::EntryObject::XRefStm(Box::new(x))),
                ..Entry::default()
            },
        );
        ctx.xref.id = Some(id);
        ctx.xref.id_xref_stm = Some(num);

        properties_add(
            &mut ctx,
            &props(&[("alpha", "one")]),
            &WriteEnv::parity(),
            &no_cancel(),
        )
        .unwrap();

        let id = ctx.xref.id.clone().unwrap();
        assert_ne!(
            format!("{:?}", id[1]),
            format!("{:?}", Object::Hex(b"BB".to_vec()))
        );
        let Some(crate::model::EntryObject::XRefStm(x)) = &ctx.xref.table[&num].object else {
            panic!("not an xref stream");
        };
        assert_eq!(
            format!("{:?}", x.stream.dict.array_entry(b"ID")),
            format!("{:?}", Some(&id))
        );
    }
}
