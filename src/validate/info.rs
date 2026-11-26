//! The document information dictionary (Go: validate/info.go).
//!
//! Relaxed validation only: the spec violations that Go collects for its notices (the version of
//! `Trapped`, a missing `ModDate` beside a `PieceInfo`) have no effect and are not collected.

use super::metadata::add_keywords;
use super::object::validate_date_object;
use crate::error::{Error, Sentinel};
use crate::model::deref::text;
use crate::model::{Version, XRefTable};
use crate::object::Object;

/// `DocumentProperty`: whether a property name may be modified (`Keywords`, `Producer`,
/// `CreationDate`, `ModDate` and `Trapped` are managed by the writer).
pub fn document_property(s: &str) -> bool {
    !matches!(
        s,
        "Keywords" | "Producer" | "CreationDate" | "ModDate" | "Trapped"
    )
}

/// `errors.Is(err, types.ErrInvalidUTF16BE)`. Only the text conversion raises it, with this
/// message (object/text.rs).
fn is_invalid_utf16be(e: &Error) -> bool {
    matches!(e.root(), Error::Corrupt(m) if m == "invalid UTF-16BE detected")
}

fn is_one_of(s: &[u8], set: &[&str]) -> bool {
    set.iter().any(|m| m.as_bytes() == s)
}

/// `validateInfoDictDate`: the date as `DateString`. Relaxed validation repairs a date that is
/// no date to the empty string.
fn validate_info_dict_date(x: &mut XRefTable, o: &Object) -> String {
    validate_date_object(x, o, Version::V10).unwrap_or_default()
}

/// `validateInfoDictTrapped`.
fn validate_info_dict_trapped(x: &mut XRefTable, o: &Object) -> Result<(), Error> {
    let o = x.dereference(o)?;
    match o {
        Object::Null | Object::Bool(_) => Ok(()),
        Object::Name(n) => {
            // Relaxed: the lower case names are accepted as well.
            if is_one_of(n, &["True", "False", "Unknown", "true", "false", "unknown"]) {
                return Ok(());
            }
            Err(Error::Validation(format!(
                "invalid <{}>",
                String::from_utf8_lossy(n)
            )))
        }
        Object::String(_) | Object::Hex(_) => {
            let s = text(o)?;
            if is_one_of(
                s.as_bytes(),
                &["True", "False", "Unknown", "true", "false", "unknown"],
            ) {
                return Ok(());
            }
            Err(Error::Validation(format!("invalid <{s}>")))
        }
        other => Err(Error::Validation(format!(
            "wrong type <{}>",
            crate::model::deref::go_v(other)
        ))),
    }
}

/// `handleProperties`: a custom entry of the info dictionary is a property of the document.
fn handle_properties(x: &mut XRefTable, key: &[u8], val: &Object) -> Result<(), Error> {
    match x.dereference_string_or_hex_literal(val, Version::V10, None) {
        Err(_) => {
            // Relaxed: any other value is ignored, if it resolves.
            x.dereference(val).map_err(|e| e.context("dereference"))?;
            Ok(())
        }
        Ok(v) => {
            if !v.is_empty() {
                // Kept for the table's life: the key (up to 3 bytes for each invalid byte), the
                // value and the entry, in a B-tree about half full.
                let n = 3 * key.len() + v.len() + 4 * size_of::<String>();
                x.held.grow_by(n as u64)?;
                x.properties
                    .insert(String::from_utf8_lossy(key).into_owned(), v);
            }
            Ok(())
        }
    }
}

/// `validateKeywords`.
fn validate_keywords(x: &mut XRefTable, v: &Object) -> Result<(), Error> {
    let s = match x.dereference_string_or_hex_literal(v, Version::V10, None) {
        Ok(s) => s,
        Err(e) => {
            x.keywords.clear();
            return Err(e.context("dereference string or hex literal"));
        }
    };
    add_keywords(x, &s);
    x.keywords = s;
    Ok(())
}

/// The text of a string entry, empty if it fails (Go assigns the result to the field before it
/// looks at the error).
fn text_of(x: &mut XRefTable, v: &Object) -> (String, Option<Error>) {
    match x.dereference_string_or_hex_literal(v, Version::V10, None) {
        Ok(s) => (s, None),
        Err(e) => (String::new(), Some(e)),
    }
}

/// `validateDocInfoDictEntryWithSpecViolations`.
fn validate_doc_info_dict_entry(x: &mut XRefTable, k: &[u8], v: &Object) -> Result<(), Error> {
    let err = match k {
        // text string, optional, since V1.1
        b"Title" => {
            let (s, e) = text_of(x, v);
            x.title = s;
            e
        }
        // text string, optional
        b"Author" => {
            let (s, e) = text_of(x, v);
            x.author = s;
            e
        }
        // text string, optional, since V1.1
        b"Subject" => {
            let (s, e) = text_of(x, v);
            x.subject = s;
            e
        }
        // text string, optional, since V1.1
        b"Keywords" => validate_keywords(x, v).err(),
        // text string, optional
        b"Creator" => {
            let (s, e) = text_of(x, v);
            x.creator = s;
            e
        }
        // text string, optional
        b"Producer" => {
            let (s, e) = text_of(x, v);
            x.producer = s;
            e
        }
        // date, optional
        b"CreationDate" => {
            x.creation_date = validate_info_dict_date(x, v);
            None
        }
        // date, required if PieceInfo is present in document catalog.
        b"ModDate" => {
            x.mod_date = validate_info_dict_date(x, v);
            None
        }
        // name, optional, since V1.3
        b"Trapped" => validate_info_dict_trapped(x, v).err(),
        b"AAPL:Keywords" => {
            x.custom_extensions = true;
            None
        }
        // text string, optional
        _ => handle_properties(x, k, v).err(),
    };
    match err {
        Some(e) => Err(e.context(format!("entry {:?}", String::from_utf8_lossy(k)))),
        None => Ok(()),
    }
}

/// `validateDocumentInfoDict`: the entries in key order. A null info object clears the
/// reference; an invalid UTF-16BE string is ignored (#264).
pub(crate) fn validate_document_info_dict(x: &mut XRefTable, obj: &Object) -> Result<(), Error> {
    let d = x
        .dereference_dict(obj)
        .map_err(|e| e.context("document info: dereference dict"))?
        .cloned();
    let Some(d) = d else {
        x.info = None;
        return Ok(());
    };
    for (k, v) in d.iter() {
        match validate_doc_info_dict_entry(x, k, v) {
            Err(e) if is_invalid_utf16be(&e) => {}
            Err(e) => return Err(e),
            Ok(()) => {}
        }
    }
    Ok(())
}

/// `validateDocumentInfoObject`. Relaxed validation clears the info reference if it refers to
/// something that is not a dictionary.
///
/// # Errors
/// The errors of the info dictionary's entries, and those of the catalog lookup.
pub fn validate_document_info_object(x: &mut XRefTable) -> Result<(), Error> {
    let Some(info) = x.info else {
        return Ok(());
    };
    if let Err(e) = validate_document_info_dict(x, &Object::Ref(info)) {
        if !e.is(Sentinel::ExpectedDict) {
            return Err(e);
        }
        x.info = None;
        return Ok(());
    }
    // `PieceInfo` without `ModDate` is a spec violation that relaxed validation digests; the
    // lookup is still made, as it can fail.
    x.catalog_has_piece_info()
        .map_err(|e| e.context("document info: catalog PieceInfo lookup"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Entry, EntryObject};
    use crate::object::{Dict, Ref, StreamDict};
    use std::sync::atomic::AtomicBool;

    fn info_ref() -> Ref {
        Ref {
            num: 1,
            generation: 0,
        }
    }

    fn string(s: &str) -> Object {
        Object::string(s)
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    /// `infoXRefTable`: object 1 is the info object, the catalog is empty, the version 2.0.
    fn info_xref_table(object: Object) -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.table.insert(1, Entry::gen0(object));
        x.info = Some(info_ref());
        x.root_dict = Some(Dict::default());
        x.header_version = Some(Version::V20);
        x
    }

    fn table_object(x: &XRefTable, n: i64) -> Object {
        match x.table.get(&n).and_then(|e| e.object.as_ref()) {
            Some(EntryObject::Obj(o)) => o.clone(),
            _ => Object::Null,
        }
    }

    fn same(a: &Object, b: &Object) -> bool {
        format!("{a:?}") == format!("{b:?}")
    }

    /// `runInfoValidationCase` for relaxed validation.
    fn run_case(
        object: Object,
        want_err: bool,
        want_info: bool,
        want_producer: &str,
        want_creation_date: &str,
    ) {
        let mut x = info_xref_table(object.clone());
        let r = validate_document_info_object(&mut x);
        assert_eq!(r.is_err(), want_err, "{r:?}");
        assert_eq!(x.info, want_info.then(info_ref));
        assert_eq!(x.producer, want_producer);
        assert_eq!(x.creation_date, want_creation_date);
        assert!(same(&table_object(&x, 1), &object));
    }

    #[test]
    fn trapped_wrong_type_prints_the_value() {
        let mut x = info_xref_table(Object::Null);
        assert_eq!(
            validate_info_dict_trapped(&mut x, &Object::Integer(3)),
            Err(Error::Validation("wrong type <3>".to_string()))
        );
    }

    /// TestValidateDocumentInfoEntriesCharacterizesStrictAndRelaxedMode, relaxed cases.
    #[test]
    fn info_entries_relaxed() {
        let malformed_utf16be = string("\\376\\377\\330\\000\\000\\101");
        run_case(
            Object::Dict(dict(&[("Producer", string("producer"))])),
            false,
            true,
            "producer",
            "",
        );
        run_case(
            Object::Dict(dict(&[("CreationDate", string("20200102030405Z"))])),
            false,
            true,
            "",
            "D:20200102030405+00'00'",
        );
        run_case(
            Object::Dict(dict(&[("CreationDate", string("not-a-date"))])),
            false,
            true,
            "",
            "",
        );
        run_case(
            Object::Dict(dict(&[("Producer", malformed_utf16be)])),
            true,
            true,
            "",
            "",
        );
        run_case(
            Object::Dict(dict(&[("Trapped", Object::Bool(true))])),
            false,
            true,
            "",
            "",
        );
        run_case(
            Object::Dict(dict(&[("Custom", Object::Integer(7))])),
            false,
            true,
            "",
            "",
        );
    }

    /// TestValidateDocumentInfoObjectReferenceRepairs, relaxed cases.
    #[test]
    fn info_reference_repairs_relaxed() {
        for (object, want_info) in [(Object::Null, false), (Object::Integer(7), false)] {
            let mut x = info_xref_table(object.clone());
            let r = validate_document_info_object(&mut x);
            assert!(r.is_ok(), "{r:?}");
            assert_eq!(x.info, want_info.then(info_ref));
            assert!(same(&table_object(&x, 1), &object));
        }
    }

    /// TestValidateDocumentInfoPieceInfoRequirement, relaxed.
    #[test]
    fn info_piece_info_without_mod_date_relaxed() {
        let d = Object::Dict(dict(&[("Producer", string("producer"))]));
        let mut x = info_xref_table(d.clone());
        x.root_dict = Some(dict(&[("PieceInfo", Object::Dict(Dict::default()))]));
        assert!(validate_document_info_object(&mut x).is_ok());
        assert_eq!(x.info, Some(info_ref()));
        assert!(same(&table_object(&x, 1), &d));
    }

    #[test]
    fn info_keywords_and_properties() {
        let d = Object::Dict(dict(&[
            ("Keywords", string("a, b;c;d")),
            ("Custom", string("value")),
            ("Title", string("T")),
            ("AAPL:Keywords", Object::Null),
        ]));
        let mut x = info_xref_table(d);
        assert!(validate_document_info_object(&mut x).is_ok());
        assert_eq!(x.title, "T");
        assert_eq!(x.keywords, "a, b;c;d");
        let k: Vec<&str> = x.keyword_list.iter().map(String::as_str).collect();
        assert_eq!(k, ["a", "b", "c", "d"]);
        assert_eq!(
            x.properties.get("Custom").map(String::as_str),
            Some("value")
        );
        assert!(x.custom_extensions);
    }

    #[test]
    fn document_property_names() {
        assert!(document_property("Title"));
        assert!(!document_property("Producer"));
    }

    fn xmp_metadata(mod_date: &str, producer: &str) -> String {
        format!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
<rdf:Description xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmlns:pdf=\"http://ns.adobe.com/pdf/1.3/\">\
<xmp:ModifyDate>{mod_date}</xmp:ModifyDate><pdf:Producer>{producer}</pdf:Producer>\
</rdf:Description></rdf:RDF></x:xmpmeta>"
        )
    }

    /// `metadataInfoXRefTable`: the info dictionary is object 1, the metadata stream object 2.
    fn metadata_info_xref_table(info_mod_date: &str, xmp_mod_date: &str) -> XRefTable {
        let info = dict(&[
            ("ModDate", string(info_mod_date)),
            ("Producer", string("info producer")),
        ]);
        let mut x = info_xref_table(Object::Dict(info));
        x.root_dict = Some(dict(&[(
            "Metadata",
            Object::Ref(Ref {
                num: 2,
                generation: 0,
            }),
        )]));
        let content = x
            .mem()
            .bytes_from(xmp_metadata(xmp_mod_date, "xmp producer").as_bytes(), 0)
            .expect("bytes");
        let sd = StreamDict {
            dict: dict(&[
                ("Type", Object::name("Metadata")),
                ("Subtype", Object::name("XML")),
            ]),
            content: Some(content),
            ..StreamDict::default()
        };
        x.table.insert(2, Entry::gen0(Object::Stream(Box::new(sd))));
        x
    }

    /// TestMetadataAuthorityControlsCachedInfoValues, relaxed cases.
    #[test]
    fn metadata_authority_controls_cached_info_values() {
        let cancel = AtomicBool::new(false);
        for (info_mod, xmp_mod, want_authoritative, want_producer) in [
            (
                "D:20200101000000+00'00'",
                "2021-01-01T00:00:00Z",
                true,
                "xmp producer",
            ),
            (
                "D:20220101000000+00'00'",
                "2021-01-01T00:00:00Z",
                false,
                "info producer",
            ),
        ] {
            let mut x = metadata_info_xref_table(info_mod, xmp_mod);
            let before = table_object(&x, 1);
            let mut v = super::super::Validator::new(&mut x, &cancel);
            let mut root = v.x.catalog().expect("catalog").clone();
            let authoritative =
                super::super::metadata_modified_after_info_dict(&mut v, &mut root).expect("order");
            assert_eq!(authoritative, want_authoritative);
            if authoritative {
                assert!(validate_document_info_object(v.x).is_ok());
                assert!(
                    super::super::metadata::validate_root_metadata(
                        &mut v,
                        &mut root,
                        false,
                        Version::V14
                    )
                    .is_ok()
                );
            } else {
                assert!(
                    super::super::metadata::validate_root_metadata(
                        &mut v,
                        &mut root,
                        false,
                        Version::V14
                    )
                    .is_ok()
                );
                assert!(validate_document_info_object(v.x).is_ok());
            }
            assert_eq!(v.x.producer, want_producer);
            assert_eq!(v.x.info, Some(info_ref()));
            assert!(same(&table_object(v.x, 1), &before));
        }
    }

    /// TestFixInfoDictClearsMetadataAlias, relaxed.
    #[test]
    fn info_reference_to_the_metadata_is_cleared() {
        let cancel = AtomicBool::new(false);
        let sd = StreamDict {
            dict: dict(&[("Type", Object::name("Metadata"))]),
            ..StreamDict::default()
        };
        let mut x = info_xref_table(Object::Stream(Box::new(sd)));
        let mut root = dict(&[("Metadata", Object::Ref(info_ref()))]);
        let mut v = super::super::Validator::new(&mut x, &cancel);
        super::super::fix_info_dict(&mut v, &mut root).expect("fix");
        assert_eq!(v.x.info, None);
    }
}
