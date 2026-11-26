//! Validation against PDF 32000-1:2008, relaxed (Go: pkg/pdfcpu/validate; docs/design.md
//! section 2). This file is `xReftable.go` without the link checks (:1408-1757): the driver, the
//! root table and the validators of the catalog entries that need no file of their own.
//!
//! # Layout
//!
//! One file per Go file or group of files, as the design lists them. Every module is declared
//! here, so a later package replaces its stub file and edits nothing else in the tree. The files
//! that exist as stubs say which package fills them in; each stub holds only the items this file
//! calls, with the signature the call needs. A package replaces a stub with the full port and
//! keeps those signatures.
//!
//! * WP 13 (this one): `mod.rs`, `state.rs`, `object.rs` (the entry validators WP 13 needs;
//!   WP 14 appends the rest of object.go to it), `info.rs`, `metadata.rs`, `xmp.rs`,
//!   `property.rs`, `viewer_prefs.rs`.
//! * WP 14: `object.rs`, `active_content.rs`, `trapnet.rs`.
//! * WP 15: `page.rs`, `optional_content.rs`. WP 16: `annotation.rs`.
//! * WP 17: `action.rs`, `destination.rs`, `file_spec.rs`. WP 18: `form.rs`, `font.rs`.
//! * WP 19: `xobject.rs`, `colorspace.rs`. WP 20: `extgstate.rs`, `function.rs`, `pattern.rs`,
//!   `shading.rs`. WP 21: `media.rs`, `three_d.rs`, `thread.rs`, `outline.rs`.
//! * WP 22: `name_tree.rs`, `number_tree.rs`, `struct_tree.rs`, and the `validate` API.
//!
//! # Conventions
//!
//! * Go's `context.Context` is [`Validator`]: the table, the cancel flag, and the values the
//!   context carries ([`state::PassState`]; a package appends the fields it needs). A function
//!   that takes a context in Go takes `&mut Validator`; one that takes only the table takes
//!   `&mut XRefTable`.
//! * Validation is relaxed, always: there is no mode parameter, and the strict branches and the
//!   notices and log output are not ported (design sections 1 and 6).
//! * `WithValidationErrorObject` is dropped (as in `model/deref.rs`), and the `ownerObjNr`
//!   parameters with it; errors keep their cause and get a context message.
//! * A dictionary, array or stream that a validator returns is an owned copy (see
//!   [`object`]). Go's validators change the stored catalog through the map they were given;
//!   here the driver validates a copy of the catalog and stores it back when the pass is done,
//!   so a root entry validator takes `&mut Dict` and changes that copy.
//! * Not stored: `XRefTable.PageLayout`, `PageMode`, `ViewerPref`, `URSignature`,
//!   `URSignatureIncrement`, `CertifiedSigObjNr` and `DSS`. Nothing the port reaches reads
//!   them. The state that is read lives in [`state::ValidateState`] (`XRefTable::vstate`).
//!
//! # What the later packages provide
//!
//! The functions below are called from this file (and, for the page and name tree ones, by the
//! catalog validators here); each package keeps the signature of its stub.
//!
//! | Function | Package |
//! |---|---|
//! | `page::validate_pages`, `page::validate_resource_dict` | 15 |
//! | `optional_content::validate_oc_properties` | 15 |
//! | `annotation::validate_pages_annotations` | 16 |
//! | `action::validate_action_dict_object`, `action::validate_root_additional_actions` | 17 |
//! | `destination::validate_destination`, `destination::validate_destination_array` | 17 |
//! | `form::validate_form_context`, `form::validate_form_fields_against_page_annotations` | 18 |
//! | `outline::validate_outlines`, `thread::validate_threads` | 21 |
//! | `name_tree::validate_name_tree`, `name_tree::validate_capture_command_dict` | 22 |
//! | `number_tree::validate_number_tree`, `struct_tree::validate_struct_tree` | 22 |
//!
//! `validate_action_dict_object` is Go's `validateActionDictObjectWithOrigin` without the origin:
//! the origin only feeds the active content notices, which are not ported.

pub mod action;
pub mod active_content;
pub mod annotation;
pub mod colorspace;
pub mod destination;
pub mod extgstate;
pub mod file_spec;
pub mod font;
pub mod form;
pub mod function;
pub mod info;
pub mod media;
pub mod metadata;
pub mod name_tree;
pub mod number_tree;
pub mod object;
pub mod optional_content;
pub mod outline;
pub mod page;
pub mod pattern;
pub mod property;
pub mod shading;
pub mod state;
pub mod struct_tree;
pub mod thread;
pub mod three_d;
pub mod trapnet;
pub mod viewer_prefs;
pub mod xmp;
pub mod xobject;

use self::info::validate_document_info_object;
use self::metadata::{catalog_meta_data, validate_root_metadata};
use self::object::{
    OPTIONAL, REQUIRED, dict_entry_context, object_context, time_of_date_object,
    validate_array_entry, validate_boolean_entry, validate_boolean_or_array_of_boolean_entry,
    validate_date_entry, validate_dict_entry, validate_entry, validate_ind_ref_array_entry,
    validate_integer_entry, validate_name_entry, validate_name_entry_keep,
    validate_name_or_array_of_name_entry, validate_number_entry_to_float,
    validate_stream_dict_entry, validate_string_entry,
};
use self::state::{PassState, RenditionPageIndex};
use self::viewer_prefs::validate_viewer_preferences;
use crate::error::Error;
use crate::ledger::Held;
use crate::model::deref::type_name;
use crate::model::equal::equal_objects;
use crate::model::{Context, Version, XRefTable};
use crate::object::{Dict, Object, dict_copy_bytes};
use std::sync::atomic::{AtomicBool, Ordering};

/// Go's `context.Context` for one validation pass, and the table it validates.
pub struct Validator<'a> {
    pub x: &'a mut XRefTable,
    pub cancel: &'a AtomicBool,
    pub st: PassState,
    /// The charge of what the pass keeps until it ends (the rendition page caches).
    pub held: Held,
}

impl<'a> Validator<'a> {
    pub fn new(x: &'a mut XRefTable, cancel: &'a AtomicBool) -> Validator<'a> {
        let held = x.mem().hold();
        Validator {
            x,
            cancel,
            st: PassState::default(),
            held,
        }
    }

    /// `contextutil.Check(c)`.
    ///
    /// # Errors
    /// [`Error::Cancelled`] once the cancel flag is set.
    pub fn check(&self) -> Result<(), Error> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        Ok(())
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `XRefTable`: validates a cross-reference table and supports cancellation. On success the
/// table is marked valid.
///
/// # Errors
/// [`Error::Cancelled`], and the validation errors ([`Error::Validation`], or the error of the
/// lookup that failed) with the path of the failing entry as context.
pub fn xref_table(ctx: &mut Context, cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    validate_xref_table(&mut ctx.xref, cancel)
}

/// `validateXRefTable`, with the working-copy reserve of the pass (docs/limits.md).
///
/// # Errors
/// As [`xref_table`], and [`Error::Alloc`] for the reserve.
pub fn validate_xref_table(x: &mut XRefTable, cancel: &AtomicBool) -> Result<(), Error> {
    let reserve = x.start_reserve()?;
    let r = validate_xref_table_in(x, cancel);
    x.end_reserve(reserve);
    r
}

fn validate_xref_table_in(x: &mut XRefTable, cancel: &AtomicBool) -> Result<(), Error> {
    let mut v = Validator::new(x, cancel);
    let mut root =
        v.x.catalog()
            .map_err(|e| e.context("load catalog"))?
            .clone();
    let r = validate_catalog(&mut v, &mut root);
    // The validators changed the catalog; Go changes the stored map through its alias.
    let stored = v.x.catalog_mut().map(|c| *c = root);
    r?;
    stored?;
    v.x.valid = true;
    Ok(())
}

fn validate_catalog(v: &mut Validator<'_>, root: &mut Dict) -> Result<(), Error> {
    validate_root_version(v.x, root, OPTIONAL, Version::V14)
        .map_err(|e| e.context("catalog version"))?;

    let metadata_authoritative = validate_document_info_before_root(v, root)?;

    // Validate root object (aka the document catalog) and page tree.
    validate_root_object(v, root).map_err(|e| e.context("catalog"))?;

    validate_document_info_after_root(v, metadata_authoritative)?;

    v.check()
}

/// `validateDocumentInfoBeforeRoot`: if the XMP metadata was modified after the info dictionary
/// it is validated first, and its values win.
fn validate_document_info_before_root(
    v: &mut Validator<'_>,
    root: &mut Dict,
) -> Result<bool, Error> {
    v.check()?;
    let metadata_authoritative =
        metadata_modified_after_info_dict(v, root).map_err(|e| e.context("metadata/info order"))?;
    if !metadata_authoritative {
        return Ok(false);
    }
    validate_document_info_object(v.x).map_err(|e| e.context("document info"))?;
    Ok(true)
}

fn validate_document_info_after_root(
    v: &mut Validator<'_>,
    metadata_authoritative: bool,
) -> Result<(), Error> {
    v.check()?;
    if metadata_authoritative {
        return Ok(());
    }
    validate_document_info_object(v.x).map_err(|e| e.context("document info"))
}

/// `fixInfoDict`: an info reference that points to the metadata stream is cleared.
///
/// # Errors
/// [`Error::Panic`] where Go dereferences a nil reference, and the errors of the comparison.
pub fn fix_info_dict(v: &mut Validator<'_>, root: &mut Dict) -> Result<(), Error> {
    let Some(meta) = root.indirect_ref_entry(b"Metadata") else {
        return Err(Error::Panic("nil Metadata reference"));
    };
    let Some(info) = v.x.info else {
        return Err(Error::Panic("nil Info reference"));
    };
    if equal_objects(v.x, &Object::Ref(meta), &Object::Ref(info), &[], v.cancel)? {
        // infoDict indRef falsely points to meta data.
        v.x.info = None;
    }
    Ok(())
}

/// `metaDataModifiedAfterInfoDict`: whether the metadata stream (not the info dictionary) holds
/// the current values. It parses the catalog's XMP and records it on the document.
fn metadata_modified_after_info_dict(
    v: &mut Validator<'_>,
    root: &mut Dict,
) -> Result<bool, Error> {
    let xmp = catalog_meta_data(v, root, OPTIONAL, Version::V14)?;

    if let Some(m) = &xmp {
        // The copy in `vstate.catalog_xmp_meta` is kept for the table's life.
        v.x.held.grow_by(m.copy_bytes())?;
        v.x.vstate.catalog_xmp_meta = xmp.clone();
        if v.x.info.is_some() {
            fix_info_dict(v, root)?;
        }
    }

    let (Some(xmp), Some(info)) = (xmp, v.x.info) else {
        return Ok(false);
    };

    let info_dict = v.x.dereference_dict_ref(info)?.cloned();
    let Some(info_dict) = info_dict else {
        return Ok(true);
    };

    let Some(mod_date) = info_dict.0.get(b"ModDate".as_slice()) else {
        return Ok(true);
    };

    let Some(info_time) = time_of_date_object(v.x, mod_date, Version::V10)? else {
        return Ok(true);
    };

    let Some(meta_time) = xmp.rdf.description.mod_date.filter(|t| !t.is_zero()) else {
        // The modification date of the metadata is zero: older than the info dictionary.
        return Ok(false);
    };

    if info_time.unix == meta_time.unix && meta_time.nanos == 0 {
        return Ok(false);
    }

    // The info dictionary is older than the metadata.
    Ok((info_time.unix, 0u32) < (meta_time.unix, meta_time.nanos))
}

/// `setRootVersion`: the version named by the catalog.
fn set_root_version(x: &mut XRefTable, s: &[u8]) -> Result<(), Error> {
    let version = match Version::parse(s) {
        Ok(v) => v,
        // Relaxed validation accepts more.
        Err(_) => Version::parse_relaxed(s)
            .map_err(|e| e.context(format!("unknown root version {}", lossy(s))))?,
    };
    x.root_version = Some(version);
    Ok(())
}

/// `validateRootVersion` (7.7.2): the optional `Version` entry of the catalog is recorded as the
/// root version, as opposed to the header version. Relaxed validation takes it as a name since
/// 1.0, and as a number if it is no name.
///
/// # Errors
/// [`Error::Validation`] for a version of 0, and the errors of the entry validators.
pub fn validate_root_version(
    x: &mut XRefTable,
    root: &Dict,
    required: bool,
    _since: Version,
) -> Result<(), Error> {
    let since = Version::V10;
    if let Ok(n) = validate_name_entry(x, root, "rootDict", "Version", required, since, None) {
        if let Some(n) = n {
            // Validate version and save corresponding constant to xRefTable.
            set_root_version(x, &n)?;
        }
        return Ok(());
    }

    let f = validate_number_entry_to_float(x, root, "rootDict", "Version", OPTIONAL, since, None)?;
    if f == 0.0 {
        return Err(Error::Validation("invalid version".to_string()));
    }
    set_root_version(x, format!("{f:.1}").as_bytes())
}

/// `validateExtensionsDirectObject`: no reference below the `Extensions` dictionary.
fn validate_extensions_direct_object(x: &XRefTable, o: &Object, depth: u64) -> Result<(), Error> {
    x.check_recursion_depth("Extensions dictionary", depth)?;
    match o {
        Object::Ref(_) => {
            return Err(Error::Validation(
                "indirect reference not permitted".to_string(),
            ));
        }
        Object::Array(a) => {
            for (i, o) in a.iter().enumerate() {
                validate_extensions_direct_object(x, o, depth + 1)
                    .map_err(|e| e.context(format!("array index {i}")))?;
            }
        }
        Object::Dict(d) => {
            for (key, o) in d.iter() {
                validate_extensions_direct_object(x, o, depth + 1)
                    .map_err(|e| e.context(format!("key {}", lossy(key))))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `validateExtensions` (7.12): the extensions dictionary is direct.
fn validate_extensions(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let (o, _) = root.entry("rootDict", b"Extensions", required)?;
    let Some(o) = o else {
        return Ok(());
    };

    if matches!(o, Object::Ref(_)) {
        return Err(Error::Validation(
            "catalog entry Extensions: value must be direct".to_string(),
        ));
    }

    v.x.validate_version("dict=rootDict entry=Extensions", since)?;

    if !matches!(o, Object::Dict(_)) {
        return Err(Error::Validation(format!(
            "dict=rootDict entry=Extensions invalid type {}",
            type_name(o)
        )));
    }

    validate_extensions_direct_object(v.x, o, 0).map_err(|e| e.context("catalog entry Extensions"))
}

/// `validatePageLabels` (12.4.2, 7.9.7): a number tree of page labels. Relaxed validation
/// digests the errors of a direct tree and of the page indices.
fn validate_page_labels(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let o = match root.find(b"PageLabels") {
        Some(o) if !o.is_null() => o.clone(),
        _ => {
            if required {
                return Err(Error::Validation(
                    "dict=rootDict required entry=PageLabels missing".to_string(),
                ));
            }
            return Ok(());
        }
    };
    let indirect = matches!(o, Object::Ref(_));
    let r = validate_page_labels_tree(v, &o, since);
    if !indirect && r.is_err() && !v.cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    r
}

fn validate_page_labels_tree(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    v.x.validate_version("PageLabels", since)?;

    let d =
        v.x.dereference_dict(o)
            .map_err(|e| e.context("PageLabels"))?
            .cloned()
            .unwrap_or_default();

    // The page indices must include 0 and be within the page count; relaxed validation
    // digests a violation.
    number_tree::validate_number_tree(v, "PageLabel", &d, true, false, Some(o))?;
    Ok(())
}

/// The names of the name trees of the name dictionary.
const NAME_TREE_NAMES: [&str; 10] = [
    "Dests",
    "AP",
    "JavaScript",
    "Pages",
    "Templates",
    "IDS",
    "URLS",
    "EmbeddedFiles",
    "AlternatePresentations",
    "Renditions",
];

/// `validateNames` (7.7.4): the name trees are validated and internalised; the `Names` entry of
/// the catalog keeps those that have keys.
fn validate_names(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(d) = validate_dict_entry(v.x, root, "rootDict", "Names", required, since, None)?
    else {
        return Ok(());
    };

    let mut d1 = Dict::default();

    for (tree_key, value) in d.iter() {
        let tree_name = lossy(tree_key);

        // Relaxed: an unknown name is skipped.
        if !NAME_TREE_NAMES.contains(&tree_name.as_str()) {
            continue;
        }

        if v.x.names.contains_key(&tree_name) {
            // Already internalized.
            continue;
        }

        let tree_dict = v.x.dereference_dict(value)?.cloned().unwrap_or_default();
        if tree_dict.is_empty() {
            continue;
        }

        let (_, _, tree) =
            name_tree::validate_name_tree(v, &tree_name, &tree_dict, true, Some(value))?;

        if let Some(tree) = tree
            && !tree.kmin.is_empty()
            && !tree.kmax.is_empty()
        {
            // Internalize.
            v.x.names.insert(tree_name, tree);
            d1.insert(tree_key, value.clone());
        }
    }

    root.delete(b"Names");
    if !d1.is_empty() {
        root.set(b"Names", Object::Dict(d1));
    }

    Ok(())
}

/// `validateNamedDestinations` (12.3.2.3): a dictionary of destinations.
fn validate_named_destinations(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let dests = validate_dict_entry(v.x, root, "rootDict", "Dests", required, since, None);
    // The copy in `vstate.dests` is kept for the table's life.
    if let Ok(Some(d)) = &dests {
        v.x.held.grow_by(dict_copy_bytes(d))?;
    }
    v.x.vstate.dests = dests.as_ref().ok().cloned().flatten();
    let Some(dests) = dests? else {
        return Ok(());
    };

    for (key, o) in dests.iter() {
        destination::validate_destination(v.x, o, false)
            .map_err(|e| e.context(format!("named destination {}", lossy(key))))?;
    }

    Ok(())
}

/// `validatePageLayout`.
fn validate_page_layout(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    // "UseNone", "Continuous", "oneside", "useoutlines" is out of spec.
    let v15 = v.x.version()? >= Version::V15;
    let valid = |s: &[u8]| {
        matches!(
            s,
            b"SinglePage"
                | b"OneColumn"
                | b"TwoColumnLeft"
                | b"TwoColumnRight"
                | b"UseNone"
                | b"Continuous"
                | b"oneside"
                | b"TwoPageRight"
                | b"useoutlines"
        ) || (v15 && s == b"TwoPageLeft")
    };
    validate_name_entry(
        v.x,
        root,
        "rootDict",
        "PageLayout",
        required,
        since,
        Some(&valid),
    )?;
    Ok(())
}

/// `nonStandardPageMode`.
fn non_standard_page_mode(s: &[u8]) -> bool {
    matches!(s, b"None" | b"none" | b"UserNone" | b"\"None\"")
}

/// `validatePageMode`: relaxed validation accepts the non-standard modes, and `UseAttachments`
/// before PDF 1.6.
fn validate_page_mode(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let version = v.x.version()?;
    let valid = |s: &[u8]| {
        matches!(
            s,
            b"UseNone" | b"UseOutlines" | b"UseThumbs" | b"FullScreen"
        ) || (version >= Version::V14 && s == b"UseOC")
            || (version >= Version::V16 && s == b"UseAttachments")
            || non_standard_page_mode(s)
    };
    match validate_name_entry_keep(
        v.x,
        root,
        "rootDict",
        "PageMode",
        required,
        since,
        Some(&valid),
    ) {
        Ok(_) => Ok(()),
        Err(e) if e.name.as_deref() == Some(b"UseAttachments".as_slice()) => Ok(()),
        Err(e) => Err(e.err),
    }
}

/// `validateOpenAction` (12.3.2, 12.6): an array defining a destination, or an action
/// dictionary.
fn validate_open_action(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let raw = root.find(b"OpenAction").cloned();
    let Some(o) = validate_entry(v.x, root, "rootDict", "OpenAction", required, since)? else {
        return Ok(());
    };

    match o {
        Object::Dict(d) => {
            action::validate_action_dict_object(v, &d, raw.as_ref(), "rootDict.OpenAction")
        }
        Object::Array(a) => destination::validate_destination_array(v.x, &a)
            .map_err(|e| e.context("rootDict.OpenAction")),
        other => Err(Error::Validation(format!(
            "rootDict entry=OpenAction expected dict or array, got {}",
            type_name(&other)
        ))),
    }
}

/// `validateURI` (12.6.4.7): a dictionary with the optional ASCII string `Base`.
fn validate_uri(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(d) = validate_dict_entry(v.x, root, "rootDict", "URI", required, since, None)? else {
        return Ok(());
    };
    validate_string_entry(v.x, &d, "URIdict", "Base", OPTIONAL, Version::V10, None)?;
    Ok(())
}

/// `validateMarkInfo` (14.7).
fn validate_mark_info(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let x = &mut *v.x;
    let Some(d) = validate_dict_entry(x, root, "rootDict", "MarkInfo", required, since, None)?
    else {
        return Ok(());
    };

    let dict_name = "markInfoDict";

    // Marked, optional, boolean
    let marked = validate_boolean_entry(x, &d, dict_name, "Marked", OPTIONAL, Version::V10, None)?;
    let mut is_tagged_pdf = marked.unwrap_or(false);

    // Suspects: optional, since V1.6 (V1.4 relaxed), boolean
    let suspects =
        validate_boolean_entry(x, &d, dict_name, "Suspects", OPTIONAL, Version::V14, None)?;
    if suspects == Some(true) {
        is_tagged_pdf = false;
    }

    x.tagged = is_tagged_pdf;

    // UserProperties: optional, since V1.6, boolean
    validate_boolean_entry(
        x,
        &d,
        dict_name,
        "UserProperties",
        OPTIONAL,
        Version::V16,
        None,
    )?;
    Ok(())
}

/// `validateLang`.
fn validate_lang(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    validate_string_entry(v.x, root, "rootDict", "Lang", required, since, None)
        .map_err(|e| e.context("rootDict.Lang"))?;
    Ok(())
}

/// `validateCaptureCommandDictArray`.
fn validate_capture_command_dict_array(x: &mut XRefTable, a: &[Object]) -> Result<(), Error> {
    for (i, o) in a.iter().enumerate() {
        let d = x
            .dereference_dict(o)
            .map_err(|e| {
                e.context(format!(
                    "{}: dereference capture command dict",
                    object_context(&format!("webCaptureInfoDict.C[{i}]"), Some(o))
                ))
            })?
            .cloned();
        let Some(d) = d else {
            continue;
        };
        name_tree::validate_capture_command_dict(x, &d).map_err(|e| {
            e.context(object_context(
                &format!("webCaptureInfoDict.C[{i}]"),
                Some(o),
            ))
        })?;
    }
    Ok(())
}

/// `validateWebCaptureInfoDict`.
fn validate_web_capture_info_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "webCaptureInfoDict";

    // V, required, since V1.3, number
    validate_number_entry_to_float(x, d, dict_name, "V", REQUIRED, Version::V13, None)
        .map_err(|e| e.context(format!("{dict_name}.V")))?;

    // C, optional, since V1.3, array of web capture command dict indRefs
    let a = validate_ind_ref_array_entry(x, d, dict_name, "C", OPTIONAL, Version::V13, None)
        .map_err(|e| e.context(format!("{dict_name}.C")))?;

    match a {
        Some(a) => validate_capture_command_dict_array(x, &a),
        None => Ok(()),
    }
}

/// `validateSpiderInfo` (14.10.2).
fn validate_spider_info(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let raw = root.find(b"SpiderInfo").cloned();
    let context = |e: Error| e.context(dict_entry_context("rootDict", "SpiderInfo", raw.as_ref()));
    let Some(d) = validate_dict_entry(v.x, root, "rootDict", "SpiderInfo", required, since, None)
        .map_err(context)?
    else {
        return Ok(());
    };
    validate_web_capture_info_dict(v.x, &d).map_err(context)
}

/// `validateOutputIntentDict`: relaxed validation does not require `S` and
/// `OutputConditionIdentifier`.
fn validate_output_intent_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "outputIntentDict";
    let v10 = Version::V10;

    // Type, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        v10,
        Some(&|s: &[u8]| s == b"OutputIntent"),
    )?;

    // S: required, name
    validate_name_entry(x, d, dict_name, "S", OPTIONAL, v10, None)?;

    // OutputCondition, optional, text string
    validate_string_entry(x, d, dict_name, "OutputCondition", OPTIONAL, v10, None)?;

    // OutputConditionIdentifier, required, text string
    validate_string_entry(
        x,
        d,
        dict_name,
        "OutputConditionIdentifier",
        OPTIONAL,
        v10,
        None,
    )?;

    // RegistryName, optional, text string
    validate_string_entry(x, d, dict_name, "RegistryName", OPTIONAL, v10, None)?;

    // Info, optional, text string
    validate_string_entry(x, d, dict_name, "Info", OPTIONAL, v10, None)?;

    // DestOutputProfile, optional, streamDict
    validate_stream_dict_entry(x, d, dict_name, "DestOutputProfile", OPTIONAL, v10, None)?;
    Ok(())
}

/// `validateOutputIntents` (14.11.5): relaxed validation takes the version as 1.3.
fn validate_output_intents(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    _since: Version,
) -> Result<(), Error> {
    let x = &mut *v.x;
    let a = validate_array_entry(
        x,
        root,
        "rootDict",
        "OutputIntents",
        required,
        Version::V13,
        None,
    )
    .map_err(|e| e.context("rootDict.OutputIntents"))?;
    let Some(a) = a else {
        return Ok(());
    };

    for (i, o) in a.iter().enumerate() {
        let d = x
            .dereference_dict(o)
            .map_err(|e| {
                e.context(format!(
                    "{}: dereference output intent dict",
                    object_context(&format!("rootDict.OutputIntents[{i}]"), Some(o))
                ))
            })?
            .cloned();
        let Some(d) = d else {
            continue;
        };
        validate_output_intent_dict(x, &d).map_err(|e| {
            e.context(object_context(
                &format!("rootDict.OutputIntents[{i}]"),
                Some(o),
            ))
        })?;
    }

    Ok(())
}

/// `validatePieceDict`: the page-piece dictionary, a dictionary of dictionaries. Relaxed
/// validation does not require `LastModified`.
///
/// # Errors
/// [`Error::Validation`] for an entry that is not valid, and the errors of the dereference.
pub fn validate_piece_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "pieceDict";

    for (name, o) in d.iter() {
        let context = || object_context(&format!("{dict_name}.{}", lossy(name)), Some(o));
        let d1 = x
            .dereference_dict(o)
            .map_err(|e| e.context(format!("{}: dereference dict", context())))?
            .cloned();
        let Some(d1) = d1 else {
            continue;
        };

        validate_date_entry(x, &d1, dict_name, "LastModified", OPTIONAL, Version::V10)
            .map_err(|e| e.context(format!("{}: LastModified", context())))?;

        validate_entry(x, &d1, dict_name, "Private", OPTIONAL, Version::V10)
            .map_err(|e| e.context(format!("{}: Private", context())))?;
    }

    Ok(())
}

/// `validatePieceInfo` (14.5): whether `d` has the page-piece dictionary `entry_name`, and is it
/// valid.
///
/// # Errors
/// As [`validate_piece_dict`].
pub fn validate_piece_info(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<bool, Error> {
    let raw = d.find(entry_name.as_bytes()).cloned();
    let context = |e: Error| e.context(dict_entry_context(dict_name, entry_name, raw.as_ref()));
    let piece_dict =
        validate_dict_entry(x, d, dict_name, entry_name, required, since, None).map_err(context)?;
    let Some(piece_dict) = piece_dict else {
        return Ok(false);
    };

    validate_piece_dict(x, &piece_dict).map_err(context)?;
    Ok(true)
}

/// `validateRootPieceInfo`: relaxed validation does not look at the catalog's `PieceInfo`.
fn validate_root_piece_info(
    _v: &mut Validator<'_>,
    _root: &mut Dict,
    _required: bool,
    _since: Version,
) -> Result<(), Error> {
    Ok(())
}

/// `indirectObjectIncrement`: the increment of the entry `obj` refers to, `fallback` for any
/// other object and for an entry that is not in the table.
pub fn indirect_object_increment(x: &XRefTable, obj: Option<&Object>, fallback: i64) -> i64 {
    let Some(Object::Ref(r)) = obj else {
        return fallback;
    };
    match x.find_table_entry_for_ind_ref(*r) {
        Some(entry) => entry.incr,
        None => fallback,
    }
}

/// `validatePermissions` (12.8.4): the `DocMDP` reference must resolve to a dictionary, and
/// `UR3` is a dictionary. The signatures are not recorded (nothing reads them).
fn validate_permissions(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let x = &mut *v.x;
    let raw = root.find(b"Perms").cloned();
    let context = dict_entry_context("rootDict", "Perms", raw.as_ref());
    let d = validate_dict_entry(x, root, "rootDict", "Perms", required, since, None)
        .map_err(|e| e.context(context.clone()))?;
    let Some(d) = d.filter(|d| !d.is_empty()) else {
        return Ok(());
    };

    if let Some(ind_ref) = d.indirect_ref_entry(b"DocMDP") {
        x.dereference_dict_ref(ind_ref).map_err(|e| {
            e.context(format!(
                "{context}: permDict.DocMDP obj#{}: dereference dict",
                ind_ref.num
            ))
        })?;
    }

    validate_dict_entry(x, &d, "permDict", "UR3", OPTIONAL, since, None)
        .map_err(|e| e.context(format!("{context}: permDict.UR3")))?;
    Ok(())
}

/// `validateLegal` (12.8.5): legal content attestations are not supported.
fn validate_legal(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let raw = root.find(b"Legal").cloned();
    let d = validate_dict_entry(v.x, root, "rootDict", "Legal", required, since, None)
        .map_err(|e| e.context(dict_entry_context("rootDict", "Legal", raw.as_ref())))?;
    match d {
        Some(d) if !d.is_empty() => Err(Error::Validation(
            "rootDict.Legal: not supported".to_string(),
        )),
        _ => Ok(()),
    }
}

/// `validateRequirementDict`.
fn validate_requirement_dict(x: &mut XRefTable, d: &Dict, since: Version) -> Result<(), Error> {
    let dict_name = "requirementDict";

    // Type, optional, name,
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"Requirement"),
    )
    .map_err(|e| e.context(format!("{dict_name}.Type")))?;

    // S, required, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "S",
        REQUIRED,
        since,
        Some(&|s: &[u8]| s == b"EnableJavaScripts"),
    )
    .map_err(|e| e.context(format!("{dict_name}.S")))?;

    // The RH entry (requirement handler dicts) shall not be used in PDF 1.7.
    Ok(())
}

/// `validateRequirements` (12.10).
fn validate_requirements(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let x = &mut *v.x;
    let a = validate_array_entry(x, root, "rootDict", "Requirements", required, since, None)
        .map_err(|e| e.context("rootDict.Requirements"))?;
    let Some(a) = a else {
        return Ok(());
    };

    for (i, o) in a.iter().enumerate() {
        let d = x
            .dereference_dict(o)
            .map_err(|e| {
                e.context(format!(
                    "{}: dereference requirement dict",
                    object_context(&format!("rootDict.Requirements[{i}]"), Some(o))
                ))
            })?
            .cloned();
        let Some(d) = d else {
            continue;
        };
        validate_requirement_dict(x, &d, since).map_err(|e| {
            e.context(object_context(
                &format!("rootDict.Requirements[{i}]"),
                Some(o),
            ))
        })?;
    }

    Ok(())
}

/// `validateCollectionFieldDict`.
fn validate_collection_field_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "colFlddict";
    let v10 = Version::V10;

    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        v10,
        Some(&|s: &[u8]| s == b"CollectionField"),
    )
    .map_err(|e| e.context(format!("{dict_name}.Type")))?;

    // Subtype, required name. See i659.pdf for the two names that relaxed validation adds.
    validate_name_entry(
        x,
        d,
        dict_name,
        "Subtype",
        REQUIRED,
        v10,
        Some(&|s: &[u8]| {
            matches!(
                s,
                b"S" | b"D"
                    | b"N"
                    | b"F"
                    | b"Desc"
                    | b"ModDate"
                    | b"CreationDate"
                    | b"Size"
                    | b"AFRelationship"
                    | b"CompressedSize"
            )
        }),
    )
    .map_err(|e| e.context(format!("{dict_name}.Subtype")))?;

    // N, required text string
    validate_string_entry(x, d, dict_name, "N", REQUIRED, v10, None)
        .map_err(|e| e.context(format!("{dict_name}.N")))?;

    // O, optional integer
    validate_integer_entry(x, d, dict_name, "O", OPTIONAL, v10, None)
        .map_err(|e| e.context(format!("{dict_name}.O")))?;

    // V, optional boolean
    validate_boolean_entry(x, d, dict_name, "V", OPTIONAL, v10, None)
        .map_err(|e| e.context(format!("{dict_name}.V")))?;

    // E, optional boolean
    validate_boolean_entry(x, d, dict_name, "E", OPTIONAL, v10, None)
        .map_err(|e| e.context(format!("{dict_name}.E")))?;

    Ok(())
}

/// `validateCollectionSchemaDict`: the entries in key order.
fn validate_collection_schema_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    for (k, v) in d.iter() {
        let context = || object_context(&format!("Collection.Schema.{}", lossy(k)), Some(v));

        if k.as_slice() == b"Type" {
            let n = x
                .dereference_name(v, Version::V10, None)
                .map_err(|e| e.context("Collection.Schema.Type: dereference name"))?;
            if n != b"CollectionSchema" {
                return Err(Error::Validation(
                    "Collection.Schema.Type: invalid value".to_string(),
                ));
            }
            continue;
        }

        let d1 = x
            .dereference_dict(v)
            .map_err(|e| e.context(format!("{}: dereference collection field dict", context())))?
            .cloned();
        let Some(d1) = d1 else {
            continue;
        };

        validate_collection_field_dict(x, &d1).map_err(|e| e.context(context()))?;
    }

    Ok(())
}

/// `validateCollectionSortDict`.
fn validate_collection_sort_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "colSortDict";

    // S, required name or array of names.
    validate_name_or_array_of_name_entry(x, d, dict_name, "S", REQUIRED, Version::V10)
        .map_err(|e| e.context(format!("{dict_name}.S")))?;

    // A, optional boolean or array of booleans.
    validate_boolean_or_array_of_boolean_entry(x, d, dict_name, "A", OPTIONAL, Version::V10)
        .map_err(|e| e.context(format!("{dict_name}.A")))?;

    Ok(())
}

/// `validateCollection` (12.3.5).
fn validate_collection(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let x = &mut *v.x;
    let raw = root.find(b"Collection").cloned();
    let context = dict_entry_context("rootDict", "Collection", raw.as_ref());
    let d = validate_dict_entry(x, root, "rootDict", "Collection", required, since, None)
        .map_err(|e| e.context(context.clone()))?;
    let Some(d) = d else {
        return Ok(());
    };

    let dict_name = "Collection";
    let fail = |e: Error, entry: &str| e.context(format!("{context}: {dict_name}.{entry}"));

    validate_name_entry(
        x,
        &d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"Collection"),
    )
    .map_err(|e| fail(e, "Type"))?;

    // Schema, optional dict
    let d1 = validate_dict_entry(x, &d, dict_name, "Schema", OPTIONAL, since, None)
        .map_err(|e| fail(e, "Schema"))?;
    if let Some(d1) = d1 {
        validate_collection_schema_dict(x, &d1).map_err(|e| fail(e, "Schema"))?;
    }

    // D, optional string
    validate_string_entry(x, &d, dict_name, "D", OPTIONAL, since, None)
        .map_err(|e| fail(e, "D"))?;

    // View, optional name
    validate_name_entry(
        x,
        &d,
        dict_name,
        "View",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| matches!(s, b"D" | b"T" | b"H" | b"C")),
    )
    .map_err(|e| fail(e, "View"))?;

    // Sort, optional dict
    let d1 = validate_dict_entry(x, &d, dict_name, "Sort", OPTIONAL, since, None)
        .map_err(|e| fail(e, "Sort"))?;
    if let Some(d1) = d1 {
        validate_collection_sort_dict(x, &d1).map_err(|e| fail(e, "Sort"))?;
    }

    Ok(())
}

/// `validateNeedsRendering`.
fn validate_needs_rendering(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    validate_boolean_entry(
        v.x,
        root,
        "rootDict",
        "NeedsRendering",
        required,
        since,
        None,
    )?;
    Ok(())
}

/// `validateDSS` (12.8.4.3): the document security store is a dictionary.
fn validate_dss(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    validate_dict_entry(v.x, root, "rootDict", "DSS", required, since, None)?;
    Ok(())
}

/// `validateAF` (14.13): associated files are not supported.
fn validate_af(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let a = validate_array_entry(v.x, root, "rootDict", "AF", required, since, None)?;
    match a {
        Some(a) if !a.is_empty() => Err(Error::Validation(
            "PDF 2.0 associated files not supported".to_string(),
        )),
        _ => Ok(()),
    }
}

/// `validateDPartRoot` (14.12): document parts are not supported.
fn validate_dpart_root(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let d = validate_dict_entry(v.x, root, "rootDict", "DPartRoot", required, since, None)?;
    match d {
        Some(d) if !d.is_empty() => Err(Error::Validation(
            "PDF 2.0 document parts not supported".to_string(),
        )),
        _ => Ok(()),
    }
}

/// A validator of one entry of the catalog: the validator, the catalog, whether the entry is
/// required and the version it is available since (`rootEntryValidator.validate`).
type RootValidator = fn(&mut Validator<'_>, &mut Dict, bool, Version) -> Result<(), Error>;

/// `rootEntryValidators`: the entries of the catalog in the order they are validated. None is
/// required (7.7.2).
const ROOT_ENTRY_VALIDATORS: [(&str, RootValidator, Version); 29] = [
    ("Extensions", validate_extensions, Version::V17),
    ("PageLabels", validate_page_labels, Version::V13),
    ("Names", validate_names, Version::V11),
    ("Dests", validate_named_destinations, Version::V11),
    (
        "ViewerPreferences",
        validate_viewer_preferences,
        Version::V12,
    ),
    ("PageLayout", validate_page_layout, Version::V10),
    ("PageMode", validate_page_mode, Version::V10),
    ("Outlines", outline::validate_outlines, Version::V10),
    ("Threads", thread::validate_threads, Version::V11),
    ("OpenAction", validate_open_action, Version::V11),
    ("AA", action::validate_root_additional_actions, Version::V14),
    ("URI", validate_uri, Version::V11),
    ("AcroForm", form::validate_form_context, Version::V12),
    ("Metadata", validate_root_metadata, Version::V14),
    (
        "StructTreeRoot",
        struct_tree::validate_struct_tree,
        Version::V13,
    ),
    ("MarkInfo", validate_mark_info, Version::V14),
    ("Lang", validate_lang, Version::V10),
    ("SpiderInfo", validate_spider_info, Version::V13),
    ("OutputIntents", validate_output_intents, Version::V14),
    ("PieceInfo", validate_root_piece_info, Version::V14),
    (
        "OCProperties",
        optional_content::validate_oc_properties,
        Version::V15,
    ),
    ("Perms", validate_permissions, Version::V15),
    ("Legal", validate_legal, Version::V17),
    ("Requirements", validate_requirements, Version::V17),
    ("Collection", validate_collection, Version::V17),
    ("NeedsRendering", validate_needs_rendering, Version::V17),
    ("DSS", validate_dss, Version::V17),
    ("AF", validate_af, Version::V20),
    ("DPartRoot", validate_dpart_root, Version::V20),
];

/// `validateRootEntries`.
fn validate_root_entries(v: &mut Validator<'_>, root: &mut Dict) -> Result<(), Error> {
    for (name, validate, since) in ROOT_ENTRY_VALIDATORS {
        v.check()?;
        if v.x.version()? < since {
            continue;
        }
        validate(v, root, OPTIONAL, since).map_err(|e| e.context(name))?;
    }
    Ok(())
}

/// `validateRootObject` (7.7.2): the document catalog and the page tree.
fn validate_root_object(v: &mut Validator<'_>, root: &mut Dict) -> Result<(), Error> {
    v.check()?;
    v.st.rendition_pages = RenditionPageIndex::default();

    // Entry               opt  since       type            info
    // ------------------------------------------------------------------------------------
    // Type                 n               string          "Catalog" (relaxed: optional)
    // Version              y   1.4         name            overrules header version if later
    // Extensions           y   ISO 32000   dict            => 7.12 Extensions Dictionary
    // Pages                n   -           (dict)          => 7.7.3 Page Tree
    // PageLabels           y   1.3         number tree     => 7.9.7 Number Trees, 12.4.2 Page Labels
    // Names                y   1.2         dict            => 7.7.4 Name Dictionary
    // Dests                y   only 1.1    (dict)          => 12.3.2.3 Named Destinations
    // ViewerPreferences    y   1.2         dict            => 12.2 Viewer Preferences
    // PageLayout           y   -           name            /SinglePage, /OneColumn etc.
    // PageMode             y   -           name            /UseNone, /FullScreen etc.
    // Outlines             y   -           (dict)          => 12.3.3 Document Outline
    // Threads              y   1.1         (array)         => 12.4.3 Articles
    // OpenAction           y   1.1         array or dict   => 12.3.2 Destinations, 12.6 Actions
    // AA                   y   1.4         dict            => 12.6.3 Trigger Events
    // URI                  y   1.1         dict            => 12.6.4.7 URI Actions
    // AcroForm             y   1.2         dict            => 12.7.2 Interactive Form Dictionary
    // Metadata             y   1.4         (stream)        => 14.3.2 Metadata Streams
    // StructTreeRoot       y   1.3         dict            => 14.7.2 Structure Hierarchy
    // Markinfo             y   1.4         dict            => 14.7 Logical Structure
    // Lang                 y   1.4         string
    // SpiderInfo           y   1.3         dict            => 14.10.2 Web Capture Information Dictionary
    // OutputIntents        y   1.4         array           => 14.11.5 Output Intents
    // PieceInfo            y   1.4         dict            => 14.5 Page-Piece Dictionaries
    // OCProperties         y   1.5         dict            => 8.11.4 Configuring Optional Content
    // Perms                y   1.5         dict            => 12.8.4 Permissions
    // Legal                y   1.5         dict            => 12.8.5 Legal Content Attestations
    // Requirements         y   1.7         array           => 12.10 Document Requirements
    // Collection           y   1.7         dict            => 12.3.5 Collections
    // NeedsRendering       y   1.7         boolean         => XML Forms Architecture (XFA) Spec.
    // DSS                  y   2.0         dict            => 12.8.4.3 Document Security Store
    // AF                   y   2.0         array of dicts  => 14.3 Associated Files
    // DPartRoot            y   2.0         dict            => 14.12 Document parts

    // Type
    validate_name_entry(
        v.x,
        root,
        "rootDict",
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"Catalog"),
    )?;

    // Pages
    let root_page_node_dict = page::validate_pages(v, root).map_err(|e| e.context("pages"))?;
    v.st.rendition_pages.finish(v.cancel)?;

    validate_root_entries(v, root)?;

    // Validate remainder of annotations after AcroForm validation only.
    annotation::validate_pages_annotations(v, &root_page_node_dict, 0)
        .map_err(|e| e.context("page annotations"))?;

    // Validate form fields against page annotations.
    v.check()?;
    if v.x.form.is_some() {
        form::validate_form_fields_against_page_annotations(v.x)
            .map_err(|e| e.context("form fields/page annotations"))?;
        // Go's `XRefTable.Form` is the catalog's `AcroForm` map: the repaired fields go there.
        form::sync_acro_form(v.x, root);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::Entry;
    use crate::object::Ref;

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn table(version: Version) -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(version);
        x
    }

    fn run(
        version: Version,
        root: &Dict,
        f: fn(&mut Validator<'_>, &mut Dict, bool, Version) -> Result<(), Error>,
        since: Version,
    ) -> Result<(), Error> {
        let mut x = table(version);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        f(&mut v, &mut root.clone(), OPTIONAL, since)
    }

    fn is_validation(r: &Result<(), Error>) -> bool {
        matches!(r, Err(e) if matches!(e.root(), Error::Validation(_)))
    }

    /// The PDF of Go's `catalogEndobjPDF(false)`.
    fn catalog_pdf(catalog: &str) -> Vec<u8> {
        let objects = [
            "<< /Type /Pages /Kids [2 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 1 0 R /MediaBox [0 0 100 100] /Resources <<>> >>".to_string(),
            catalog.to_string(),
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

    #[test]
    fn a_read_document_validates() {
        let cancel = AtomicBool::new(false);
        let pdf = catalog_pdf("<< /Type /Catalog /Pages 1 0 R /Lang (en) /Version /1.7 >>");
        let mut ctx = crate::api::read(&pdf, &Config::default(), &cancel).expect("read");
        assert!(!ctx.xref.valid);
        xref_table(&mut ctx, &cancel).expect("validate");
        assert!(ctx.xref.valid);
        assert_eq!(ctx.xref.root_version, Some(Version::V17));
    }

    #[test]
    fn an_invalid_catalog_entry_fails_with_its_path() {
        let cancel = AtomicBool::new(false);
        let pdf = catalog_pdf("<< /Type /Catalog /Pages 1 0 R /NeedsRendering (x) >>");
        let mut ctx = crate::api::read(&pdf, &Config::default(), &cancel).expect("read");
        let err = xref_table(&mut ctx, &cancel).expect_err("invalid");
        assert!(matches!(err.root(), Error::Validation(_)), "{err:?}");
        assert!(err.to_string().contains("NeedsRendering"), "{err}");
        assert!(!ctx.xref.valid);
    }

    #[test]
    fn a_cancelled_pass_stops() {
        let cancelled = AtomicBool::new(true);
        let pdf = catalog_pdf("<< /Type /Catalog /Pages 1 0 R >>");
        let mut ctx =
            crate::api::read(&pdf, &Config::default(), &AtomicBool::new(false)).expect("read");
        assert_eq!(xref_table(&mut ctx, &cancelled), Err(Error::Cancelled));
    }

    #[test]
    fn the_root_version_is_a_name_or_a_number() {
        let mut x = table(Version::V14);
        let root = dict(&[("Version", Object::name("1.5"))]);
        validate_root_version(&mut x, &root, OPTIONAL, Version::V14).expect("name");
        assert_eq!(x.root_version, Some(Version::V15));

        // Relaxed: a number.
        let root = dict(&[("Version", Object::Float(1.6))]);
        validate_root_version(&mut x, &root, OPTIONAL, Version::V14).expect("number");
        assert_eq!(x.root_version, Some(Version::V16));

        let root = dict(&[("Version", Object::Integer(0))]);
        assert!(validate_root_version(&mut x, &root, OPTIONAL, Version::V14).is_err());
        let root = dict(&[("Version", Object::name("9.9"))]);
        assert!(validate_root_version(&mut x, &root, OPTIONAL, Version::V14).is_err());
        // No entry, no change.
        let root = Dict::default();
        validate_root_version(&mut x, &root, OPTIONAL, Version::V14).expect("none");
        assert_eq!(x.root_version, Some(Version::V16));
    }

    #[test]
    fn extensions_are_direct() {
        let ext = Object::Dict(dict(&[(
            "ADBE",
            Object::Dict(dict(&[("BaseVersion", Object::name("1.7"))])),
        )]));
        let root = dict(&[("Extensions", ext)]);
        assert_eq!(
            run(Version::V17, &root, validate_extensions, Version::V17),
            Ok(())
        );

        let nested_ref = Object::Dict(dict(&[(
            "ADBE",
            Object::Array(vec![Object::Ref(Ref {
                num: 1,
                generation: 0,
            })]),
        )]));
        let root = dict(&[("Extensions", nested_ref)]);
        assert!(is_validation(&run(
            Version::V17,
            &root,
            validate_extensions,
            Version::V17
        )));

        let indirect = dict(&[(
            "Extensions",
            Object::Ref(Ref {
                num: 1,
                generation: 0,
            }),
        )]);
        assert!(is_validation(&run(
            Version::V17,
            &indirect,
            validate_extensions,
            Version::V17
        )));

        let not_dict = dict(&[("Extensions", Object::Integer(1))]);
        assert!(is_validation(&run(
            Version::V17,
            &not_dict,
            validate_extensions,
            Version::V17
        )));
    }

    #[test]
    fn page_mode_and_layout() {
        let mode = |m: &str, version| {
            run(
                version,
                &dict(&[("PageMode", Object::name(m))]),
                validate_page_mode,
                Version::V10,
            )
        };
        assert_eq!(mode("FullScreen", Version::V14), Ok(()));
        assert_eq!(mode("none", Version::V14), Ok(()));
        assert_eq!(mode("UseAttachments", Version::V14), Ok(()));
        assert!(is_validation(&mode("Other", Version::V17)));
        assert!(is_validation(&mode("UseOC", Version::V13)));
        assert_eq!(mode("UseOC", Version::V14), Ok(()));

        let layout = |l: &str, version| {
            run(
                version,
                &dict(&[("PageLayout", Object::name(l))]),
                validate_page_layout,
                Version::V10,
            )
        };
        assert_eq!(layout("OneColumn", Version::V14), Ok(()));
        assert_eq!(layout("TwoPageLeft", Version::V15), Ok(()));
        assert!(is_validation(&layout("TwoPageLeft", Version::V14)));
    }

    #[test]
    fn mark_info_sets_tagged() {
        let mut x = table(Version::V17);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let mark = |marked, suspects| {
            let mut m = dict(&[("Marked", Object::Bool(marked))]);
            if let Some(s) = suspects {
                m.set(b"Suspects", Object::Bool(s));
            }
            dict(&[("MarkInfo", Object::Dict(m))])
        };
        validate_mark_info(&mut v, &mut mark(true, None), OPTIONAL, Version::V14).expect("marked");
        assert!(v.x.tagged);
        validate_mark_info(&mut v, &mut mark(true, Some(true)), OPTIONAL, Version::V14)
            .expect("suspects");
        assert!(!v.x.tagged);
    }

    #[test]
    fn output_intents_and_requirements() {
        let intent = |entries: &[(&str, Object)]| {
            dict(&[(
                "OutputIntents",
                Object::Array(vec![Object::Dict(dict(entries))]),
            )])
        };
        let ok = intent(&[
            ("Type", Object::name("OutputIntent")),
            ("S", Object::name("GTS_PDFX")),
            ("OutputConditionIdentifier", Object::string("CGATS TR 001")),
        ]);
        assert_eq!(
            run(Version::V17, &ok, validate_output_intents, Version::V14),
            Ok(())
        );
        let bad = intent(&[("Type", Object::name("Other"))]);
        assert!(is_validation(&run(
            Version::V17,
            &bad,
            validate_output_intents,
            Version::V14
        )));

        let req = |s: &str| {
            dict(&[(
                "Requirements",
                Object::Array(vec![Object::Dict(dict(&[("S", Object::name(s))]))]),
            )])
        };
        assert_eq!(
            run(
                Version::V17,
                &req("EnableJavaScripts"),
                validate_requirements,
                Version::V17
            ),
            Ok(())
        );
        assert!(is_validation(&run(
            Version::V17,
            &req("Other"),
            validate_requirements,
            Version::V17
        )));
    }

    #[test]
    fn collection() {
        let coll = |schema: Object| {
            dict(&[(
                "Collection",
                Object::Dict(dict(&[
                    ("Type", Object::name("Collection")),
                    ("View", Object::name("T")),
                    ("Schema", schema),
                ])),
            )])
        };
        let field = Object::Dict(dict(&[
            ("Subtype", Object::name("S")),
            ("N", Object::string("Name")),
        ]));
        let schema = Object::Dict(dict(&[
            ("Type", Object::name("CollectionSchema")),
            ("Title", field),
        ]));
        assert_eq!(
            run(
                Version::V17,
                &coll(schema),
                validate_collection,
                Version::V17
            ),
            Ok(())
        );
        let bad_schema = Object::Dict(dict(&[("Type", Object::name("Other"))]));
        assert!(is_validation(&run(
            Version::V17,
            &coll(bad_schema),
            validate_collection,
            Version::V17
        )));
        let no_subtype = Object::Dict(dict(&[(
            "Title",
            Object::Dict(dict(&[("N", Object::string("x"))])),
        )]));
        assert!(is_validation(&run(
            Version::V17,
            &coll(no_subtype),
            validate_collection,
            Version::V17
        )));
    }

    #[test]
    fn unsupported_and_simple_entries() {
        let nonempty = Object::Dict(dict(&[("A", Object::Null)]));
        let legal = dict(&[("Legal", nonempty.clone())]);
        assert!(is_validation(&run(
            Version::V17,
            &legal,
            validate_legal,
            Version::V17
        )));
        let empty_legal = dict(&[("Legal", Object::Dict(Dict::default()))]);
        assert_eq!(
            run(Version::V17, &empty_legal, validate_legal, Version::V17),
            Ok(())
        );
        let af = dict(&[("AF", Object::Array(vec![Object::Null]))]);
        assert!(is_validation(&run(
            Version::V20,
            &af,
            validate_af,
            Version::V20
        )));
        let dpart = dict(&[("DPartRoot", nonempty)]);
        assert!(is_validation(&run(
            Version::V20,
            &dpart,
            validate_dpart_root,
            Version::V20
        )));
        let needs = dict(&[("NeedsRendering", Object::Bool(true))]);
        assert_eq!(
            run(Version::V17, &needs, validate_needs_rendering, Version::V17),
            Ok(())
        );
        let dss = dict(&[("DSS", Object::Integer(1))]);
        assert!(is_validation(&run(
            Version::V20,
            &dss,
            validate_dss,
            Version::V17
        )));
    }

    #[test]
    fn perms_and_increments() {
        let mut x = table(Version::V17);
        let mut e = Entry::gen0(Object::Dict(Dict::default()));
        e.incr = 3;
        x.table.insert(5, e);
        let r5 = Object::Ref(Ref {
            num: 5,
            generation: 0,
        });
        assert_eq!(indirect_object_increment(&x, Some(&r5), 1), 3);
        assert_eq!(
            indirect_object_increment(&x, Some(&Object::Integer(1)), 1),
            1
        );
        assert_eq!(
            indirect_object_increment(
                &x,
                Some(&Object::Ref(Ref {
                    num: 9,
                    generation: 0
                })),
                2
            ),
            2
        );

        let perms = dict(&[(
            "Perms",
            Object::Dict(dict(&[
                ("DocMDP", r5),
                ("UR3", Object::Dict(dict(&[("A", Object::Null)]))),
            ])),
        )]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        assert!(validate_permissions(&mut v, &mut perms.clone(), OPTIONAL, Version::V15).is_ok());
        let bad = dict(&[("Perms", Object::Integer(1))]);
        assert!(is_validation(&validate_permissions(
            &mut v,
            &mut bad.clone(),
            OPTIONAL,
            Version::V15
        )));
    }

    #[test]
    fn page_labels_are_digested_when_direct() {
        let direct = dict(&[("PageLabels", Object::Integer(1))]);
        assert_eq!(
            run(Version::V17, &direct, validate_page_labels, Version::V13),
            Ok(())
        );
        let indirect = dict(&[(
            "PageLabels",
            Object::Ref(Ref {
                num: 7,
                generation: 0,
            }),
        )]);
        // An undefined target dereferences to an empty dict (Go: nil Dict), which has neither
        // Kids nor Nums; an indirect tree's error is not digested.
        assert!(is_validation(&run(
            Version::V17,
            &indirect,
            validate_page_labels,
            Version::V13
        )));
        // The version check is an error for an indirect tree only.
        assert!(is_validation(&run(
            Version::V12,
            &indirect,
            validate_page_labels,
            Version::V13
        )));
        assert_eq!(
            run(Version::V12, &direct, validate_page_labels, Version::V13),
            Ok(())
        );
    }

    #[test]
    fn uri_lang_and_spider_info() {
        let uri = dict(&[(
            "URI",
            Object::Dict(dict(&[("Base", Object::string("http://a"))])),
        )]);
        assert_eq!(run(Version::V17, &uri, validate_uri, Version::V11), Ok(()));
        let bad_uri = dict(&[("URI", Object::Dict(dict(&[("Base", Object::Integer(1))])))]);
        assert!(is_validation(&run(
            Version::V17,
            &bad_uri,
            validate_uri,
            Version::V11
        )));
        let lang = dict(&[("Lang", Object::string("en-US"))]);
        assert_eq!(
            run(Version::V17, &lang, validate_lang, Version::V10),
            Ok(())
        );
        let bad_lang = dict(&[("Lang", Object::name("en"))]);
        assert!(is_validation(&run(
            Version::V17,
            &bad_lang,
            validate_lang,
            Version::V10
        )));
        // The web capture version is required.
        let spider = dict(&[("SpiderInfo", Object::Dict(Dict::default()))]);
        assert!(is_validation(&run(
            Version::V17,
            &spider,
            validate_spider_info,
            Version::V13
        )));
        let spider = dict(&[(
            "SpiderInfo",
            Object::Dict(dict(&[("V", Object::Integer(1))])),
        )]);
        assert_eq!(
            run(Version::V17, &spider, validate_spider_info, Version::V13),
            Ok(())
        );
    }
}
