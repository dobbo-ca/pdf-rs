//! Metadata streams (Go: validate/metaData.go). The XMP packet is parsed by [`super::xmp`].

use super::Validator;
use super::object::{
    OPTIONAL, dict_entry_context, validate_name_entry, validate_stream_dict_entry,
};
use super::xmp::{XmpMeta, parse_xmp};
use crate::error::Error;
use crate::model::{Version, XRefTable};
use crate::object::{Dict, StreamDict};

/// `validateMetadataStream`: the metadata stream of the `Metadata` entry of `d`, `None` if there
/// is none. Relaxed validation takes the version as 1.0 and does not require `Type` and
/// `Subtype`, so `_since` is not used. If the entry refers to nothing it is deleted from `d`
/// (the caller's copy: write it back where Go's alias would have persisted the deletion).
///
/// # Errors
/// [`Error::Validation`] for a `Type` that is not `Metadata` or a `Subtype` that is not `XML`
/// or `XMP`, and the errors of `validate_stream_dict_entry`.
pub fn validate_metadata_stream(
    x: &mut XRefTable,
    d: &mut Dict,
    required: bool,
    _since: Version,
) -> Result<Option<StreamDict>, Error> {
    let raw = d.find(b"Metadata").cloned();
    let sd = validate_stream_dict_entry(x, d, "dict", "Metadata", required, Version::V10, None)
        .map_err(|e| e.context(dict_entry_context("dict", "Metadata", raw.as_ref())))?;
    let Some(sd) = sd else {
        d.delete(b"Metadata");
        return Ok(None);
    };

    let dict_name = "metaDataDict";
    validate_name_entry(
        x,
        &sd.dict,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"Metadata"),
    )
    .map_err(|e| e.context(format!("{dict_name}.Type")))?;

    validate_name_entry(
        x,
        &sd.dict,
        dict_name,
        "Subtype",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"XML" || s == b"XMP"),
    )
    .map_err(|e| e.context(format!("{dict_name}.Subtype")))?;

    Ok(Some(sd))
}

/// `validateMetadata` (14.3): any PDF stream or dictionary may have metadata attached.
///
/// # Errors
/// As [`validate_metadata_stream`].
pub fn validate_metadata(
    x: &mut XRefTable,
    d: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    validate_metadata_stream(x, d, required, since)?;
    Ok(())
}

/// `catalogMetaData`: the parsed XMP packet of the catalog's metadata stream. `None` if there is
/// no stream, if it uses a filter that is not supported, or if the packet does not parse
/// (relaxed validation skips it).
///
/// # Errors
/// [`Error::Validation`] and the errors of [`validate_metadata_stream`]; the decode errors of
/// the stream; [`Error::Alloc`], [`Error::OutOfMemory`] and [`Error::Cancelled`] from the parse
/// (they are not a packet that fails to parse).
pub fn catalog_meta_data(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<Option<XmpMeta>, Error> {
    let sd = validate_metadata_stream(v.x, root, required, since)
        .map_err(|e| e.context("catalog metadata stream"))?;
    let Some(mut sd) = sd else {
        return Ok(None);
    };

    // Decode for supported filters only. Go compares with `==`, which no wrapped error
    // satisfies, so this never matches the errors of the decode pipeline.
    match v.x.decode_stream(&mut sd, v.cancel) {
        Ok(()) => {}
        Err(Error::UnsupportedFilter(_)) => return Ok(None),
        Err(e) => return Err(e.context("decode catalog metadata")),
    }

    let Some(content) = sd.content.as_deref() else {
        return Ok(None);
    };
    parse_xmp(content, v.x.mem(), v.cancel)
}

/// `populateKeywordList` and the loop of `validateKeywords`: the keywords of `keywords`, split
/// at `,`, `;` and `\r`, trimmed, go into the keyword list of the document.
pub fn add_keywords(x: &mut XRefTable, keywords: &str) {
    for s in keywords.split([',', ';', '\r']).filter(|s| !s.is_empty()) {
        x.keyword_list.insert(s.trim().to_string());
    }
}

/// `validateRootMetadata`: the document properties of the XMP packet found by
/// `metaDataModifiedAfterInfoDict` override those of the info dictionary.
///
/// # Errors
/// [`Error::Alloc`] for the copies it makes and keeps.
pub fn validate_root_metadata(
    v: &mut Validator<'_>,
    _root: &mut Dict,
    _required: bool,
    _since: Version,
) -> Result<(), Error> {
    let Some(meta) = v.x.vstate.catalog_xmp_meta.as_ref() else {
        return Ok(());
    };
    // The working copy, and what the table keeps of it: the joined title, author and subject, the
    // creator and the producer (at most the copy), and the keyword list (a `String` per keyword,
    // in a B-tree about half full, and a keyword takes 2 bytes of the text but the last).
    let n = meta.copy_bytes();
    let keyword_list =
        (meta.rdf.description.keywords.len() as u64 + 2) * size_of::<String>() as u64;
    let _copy = v.x.mem().charge(n)?;
    v.x.held.grow_by(n + keyword_list)?;
    let Some(meta) = v.x.vstate.catalog_xmp_meta.clone() else {
        return Ok(());
    };
    let d = &meta.rdf.description;
    let x = &mut *v.x;

    let s = d.title.join(", ");
    if !s.is_empty() || x.title.is_empty() {
        x.title = s;
    }

    let s = d.author.join(", ");
    if !s.is_empty() || x.author.is_empty() {
        x.author = s;
    }

    let s = d.subject.join(", ");
    if !s.is_empty() || x.subject.is_empty() {
        x.subject = s;
    }

    if !d.creator.is_empty() || x.creator.is_empty() {
        x.creator = d.creator.clone();
    }

    if let Some(t) = d.creation_date.filter(|t| !t.is_zero()) {
        x.creation_date = t.go_time().date_string();
    }

    if let Some(t) = d.mod_date.filter(|t| !t.is_zero()) {
        x.mod_date = t.go_time().date_string();
    }

    if !d.producer.is_empty() || x.producer.is_empty() {
        x.producer = d.producer.clone();
    }

    add_keywords(x, &d.keywords);

    Ok(())
}
