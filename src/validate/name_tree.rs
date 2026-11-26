//! Name trees (Go: validate/nameTree.go).
//!
//! Relaxed validation only. What Go digests as a spec violation (keys out of lexical order, a
//! node whose `/Limits` do not hold its keys, an empty `/Kids`) has no effect and is not
//! collected. In relaxed mode a kid that fails is skipped, except for the recursion limit, a
//! cycle or a duplicate node, and cancellation.
//!
//! The values of the trees that are not collected for notices are validated as Go does: the
//! destinations, appearance streams, JavaScript actions, pages, templates, web capture content
//! sets, embedded files, rich media assets, slide shows, renditions and the structure elements
//! of an ID tree. Go's active content notices and link collection are not ported.
//!
//! Differences from Go:
//!
//! * [`Node::d`] and the dictionaries of the table are separate copies. Go's `d["Names"] = a`
//!   (nameTree.go:1099) changes the stored node dictionary, and `a[i] = key` the stored array;
//!   here both are written back when the node was reached through a reference
//!   ([`store_dict_entry`], [`store_object`]). A tree root or kid that is a direct dictionary
//!   inside another object is not stored back.
//! * The JavaScript tree shares one action visit between its values in Go, so a shared action
//!   chain is validated once. Here each value validates its action on its own, which gives the
//!   same result.
//! * `Node.Value` (model/nameTree.go:77) is only needed to find the start resource of a slide
//!   show; [`node_has_key`] does that.

use super::Validator;
use super::action::{validate_action_dict_object, validate_java_script_action_dict};
use super::destination::validate_destination;
use super::file_spec::{
    is_embedded_file_specification, is_url_file_specification,
    validate_file_specification_without_link_collection,
};
use super::media::validate_rendition_dict;
use super::number_tree::{
    check_validation_tree, store_dict_entry, store_object, wrap_recursion_error,
};
use super::object::{
    OPTIONAL, REQUIRED, TreeVisit, fatal_tree_error, validate_array_pairs, validate_date_entry,
    validate_dict_entry, validate_entry, validate_ind_ref_array_entry, validate_ind_ref_entry,
    validate_integer_entry, validate_integer_or_array_of_integer_entry, validate_name_entry,
    validate_string_array_entry, validate_string_entry, validate_string_or_stream_entry,
};
use super::struct_tree::validate_struct_element_dict_context;
use super::xobject::validate_xobject_stream_dict;
use crate::error::Error;
use crate::model::deref::{text, type_name};
use crate::model::{Node, Version, XRefTable};
use crate::object::string::{hex_decode, unescape};
use crate::object::text::string_or_hex_literal;
use crate::object::{Dict, Object, array_copy_bytes, dict_copy_bytes};
use std::collections::BTreeSet;

/// What Go keeps in the context of one name tree walk (`javaScriptNameTreeVisit`,
/// `webCaptureNameTreeVisit`): the values that were validated already.
#[derive(Debug, Default)]
struct Scope {
    js_values: BTreeSet<i64>,
    wc_values: BTreeSet<i64>,
    wc_content_sets: BTreeSet<i64>,
}

/// The object number of `o` if it is a reference, else 0 (`validationObjectNumber(0, o)`).
fn ref_num(o: &Object) -> i64 {
    match o {
        Object::Ref(r) => r.num,
        _ => 0,
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn validation(msg: impl Into<String>) -> Error {
    Error::Validation(msg.into())
}

/// `validateDestsNameTreeValue`.
fn validate_dests_name_tree_value(
    x: &mut XRefTable,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    x.validate_version("DestsNameTreeValue", since)?;
    validate_destination(x, o, false)?;
    Ok(())
}

/// `validateAPNameTreeValue`.
fn validate_ap_name_tree_value(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    v.x.validate_version("APNameTreeValue", since)?;
    validate_xobject_stream_dict(v, o)
}

/// `validateJavaScriptNameTreeValue` (12.6.4.17).
fn validate_java_script_name_tree_value(
    v: &mut Validator<'_>,
    scope: &mut Scope,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    v.check()?;
    v.x.validate_version("JavaScriptNameTreeValue", since)?;

    let obj_nr = ref_num(o);
    if scope.js_values.contains(&obj_nr) {
        return Ok(());
    }

    let d =
        v.x.dereference_dict(o)
            .map_err(|e| e.context("JavaScript name tree value: dereference dict"))?
            .cloned()
            .unwrap_or_default();

    // S, required in strict mode, name
    let s = validate_name_entry(
        v.x,
        &d,
        "JavaScript",
        "S",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"JavaScript"),
    )?;
    if s.is_none() {
        validate_java_script_action_dict(v.x, &d, "JavaScript")?;
    }

    validate_action_dict_object(v, &d, Some(o), "JavaScript name tree value")?;
    if obj_nr > 0 {
        scope.js_values.insert(obj_nr);
    }
    Ok(())
}

/// `validate3DResourcesNameTreeValue`.
fn validate_3d_resources_name_tree_value(
    x: &mut XRefTable,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    x.validate_version("3DResourcesNameTreeValue", since)?;
    x.dereference(o)?;
    Ok(())
}

/// `validatePagesNameTreeValue` (12.7.6): a page dictionary.
fn validate_pages_name_tree_value(
    x: &mut XRefTable,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    x.validate_version("PagesNameTreeValue", since)?;

    let d = x
        .dereference_dict(o)
        .map_err(|e| e.context("Pages name tree value: dereference page dict"))?
        .cloned();
    let Some(d) = d else {
        return Err(validation("Pages name tree value: missing page dict"));
    };

    validate_name_entry(
        x,
        &d,
        "pageDict",
        "Type",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Page"),
    )?;
    Ok(())
}

/// `validateTemplatesNameTreeValue` (12.7.6): a template dictionary.
fn validate_templates_name_tree_value(
    x: &mut XRefTable,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    x.validate_version("TemplatesNameTreeValue", since)?;

    let d = x
        .dereference_dict(o)
        .map_err(|e| e.context("Templates name tree value: dereference template dict"))?
        .cloned();
    let Some(d) = d else {
        return Err(validation(
            "Templates name tree value: missing template dict",
        ));
    };

    validate_name_entry(
        x,
        &d,
        "templateDict",
        "Type",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Template"),
    )?;
    Ok(())
}

/// `validateURLAliasDict`.
fn validate_url_alias_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "urlAliasDict";

    // U, required, ASCII string
    validate_string_entry(x, d, dict_name, "U", REQUIRED, Version::V10, None)?;

    // C, optional, array of strings
    validate_string_array_entry(x, d, dict_name, "C", OPTIONAL, Version::V10, None)?;
    Ok(())
}

/// `validateCommandSettingsDict` (14.10.5.4).
fn validate_command_settings_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "cmdSettingsDict";

    // G, optional, dict
    validate_dict_entry(x, d, dict_name, "G", OPTIONAL, Version::V10, None)?;

    // C, optional, dict
    validate_dict_entry(x, d, dict_name, "C", OPTIONAL, Version::V10, None)?;
    Ok(())
}

/// `validateCaptureCommandDict` (nameTree.go:213): a web capture command dictionary.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_capture_command_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "captureCommandDict";
    let v10 = Version::V10;

    // URL, required, string
    validate_string_entry(x, d, dict_name, "URL", REQUIRED, v10, None)
        .map_err(|e| e.context(format!("{dict_name}.URL")))?;

    // L, optional, integer
    validate_integer_entry(x, d, dict_name, "L", OPTIONAL, v10, None)
        .map_err(|e| e.context(format!("{dict_name}.L")))?;

    // F, optional, integer
    validate_integer_entry(x, d, dict_name, "F", OPTIONAL, v10, None)
        .map_err(|e| e.context(format!("{dict_name}.F")))?;

    // P, optional, string or stream
    validate_string_or_stream_entry(x, d, dict_name, "P", OPTIONAL, v10)
        .map_err(|e| e.context(format!("{dict_name}.P")))?;

    // CT, optional, ASCII string
    validate_string_entry(x, d, dict_name, "CT", OPTIONAL, v10, None)
        .map_err(|e| e.context(format!("{dict_name}.CT")))?;

    // H, optional, string
    validate_string_entry(x, d, dict_name, "H", OPTIONAL, v10, None)
        .map_err(|e| e.context(format!("{dict_name}.H")))?;

    // S, optional, command settings dict
    let d1 = validate_dict_entry(x, d, dict_name, "S", OPTIONAL, v10, None)
        .map_err(|e| e.context(format!("{dict_name}.S")))?;
    if let Some(d1) = d1 {
        validate_command_settings_dict(x, &d1).map_err(|e| e.context(format!("{dict_name}.S")))?;
    }
    Ok(())
}

/// `validateSourceInfoDictEntryAU`: a string or a URL alias dictionary.
fn validate_source_info_dict_entry_au(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = validate_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };

    match &o {
        Object::String(_) | Object::Hex(_) => {
            text(&o).map_err(|e| e.context(format!("dict={dict_name} entry={entry_name}")))?;
        }
        Object::Dict(d1) => validate_url_alias_dict(x, d1)?,
        other => {
            return Err(validation(format!(
                "dict={dict_name} entry={entry_name} expected string or dict, got {}",
                type_name(other)
            )));
        }
    }
    Ok(())
}

/// `validateSourceInfoDict`.
fn validate_source_info_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "sourceInfoDict";
    let v10 = Version::V10;

    // AU, required, ASCII string or dict
    validate_source_info_dict_entry_au(x, d, dict_name, "AU", REQUIRED, v10)?;

    // E, optional, date
    validate_date_entry(x, d, dict_name, "E", OPTIONAL, v10)?;

    // S, optional, integer
    validate_integer_entry(
        x,
        d,
        dict_name,
        "S",
        OPTIONAL,
        v10,
        Some(&|i: i64| (0..=2).contains(&i)),
    )?;

    // C, optional, indRef of command dict
    if let Some(ir) = validate_ind_ref_entry(x, d, dict_name, "C", OPTIONAL, v10)? {
        let d1 = x
            .dereference_dict(&Object::Ref(ir))?
            .cloned()
            .unwrap_or_default();
        validate_capture_command_dict(x, &d1)?;
    }
    Ok(())
}

/// `validateEntrySI` (14.10.5, table 355): a source information dictionary or an array of them.
fn validate_entry_si(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    let Some(o) = validate_entry(x, d, dict_name, entry_name, required, since)? else {
        return Ok(());
    };

    match &o {
        Object::Dict(d1) => validate_source_info_dict(x, d1)?,
        Object::Array(a) => {
            for (i, e) in a.iter().enumerate() {
                if e.is_null() {
                    continue;
                }
                let d1 = x
                    .dereference_dict(e)
                    .map_err(|err| {
                        err.context(format!(
                            "dict={dict_name} entry={entry_name}[{i}]: dereference dict"
                        ))
                    })?
                    .cloned()
                    .unwrap_or_default();
                validate_source_info_dict(x, &d1).map_err(|err| {
                    err.context(format!("dict={dict_name} entry={entry_name}[{i}]"))
                })?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `validateWebCaptureContentSetDict` (14.10.4).
fn validate_web_capture_content_set_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "webCaptureContentSetDict";
    let v10 = Version::V10;

    // Type, optional, name
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        OPTIONAL,
        v10,
        Some(&|s: &[u8]| s == b"SpiderContentSet"),
    )?;

    // S, required, name
    let s = validate_name_entry(
        x,
        d,
        dict_name,
        "S",
        REQUIRED,
        v10,
        Some(&|s: &[u8]| s == b"SPS" || s == b"SIS"),
    )?
    .unwrap_or_default();

    // ID, required, byte string
    validate_string_entry(x, d, dict_name, "ID", REQUIRED, v10, None)?;

    // O, required, array of indirect references.
    validate_ind_ref_array_entry(x, d, dict_name, "O", REQUIRED, v10, None)?;

    // SI, required, source info dict or array of source info dicts
    validate_entry_si(x, d, dict_name, "SI", REQUIRED, v10)?;

    // CT, optional, string
    validate_string_entry(x, d, dict_name, "CT", OPTIONAL, v10, None)?;

    // TS, optional, date
    validate_date_entry(x, d, dict_name, "TS", OPTIONAL, v10)?;

    // spider page set
    if s == b"SPS" {
        // T, optional, string
        validate_string_entry(x, d, dict_name, "T", OPTIONAL, v10, None)?;

        // TID, optional, byte string
        validate_string_entry(x, d, dict_name, "TID", OPTIONAL, v10, None)?;
    }

    // spider image set
    if s == b"SIS" {
        // R, required, integer or array of integers
        validate_integer_or_array_of_integer_entry(x, d, dict_name, "R", REQUIRED, v10)?;
    }
    Ok(())
}

/// `validateWebCaptureContentSet`: a content set dictionary, validated once per object.
fn validate_web_capture_content_set(
    x: &mut XRefTable,
    o: &Object,
    validated: &mut BTreeSet<i64>,
) -> Result<(), Error> {
    // see 14.10.4
    let obj_nr = ref_num(o);
    if validated.contains(&obj_nr) {
        return Ok(());
    }
    let d = x
        .dereference_dict(o)
        .map_err(|e| e.context("dereference content set dict"))?
        .cloned();
    let Some(d) = d else {
        return Err(validation("missing content set dict"));
    };
    validate_web_capture_content_set_dict(x, &d)?;
    if obj_nr > 0 {
        validated.insert(obj_nr);
    }
    Ok(())
}

/// `validateWebCaptureNameTreeValue` (14.10.3.1): a content set or an array of them.
fn validate_web_capture_name_tree_value(
    v: &mut Validator<'_>,
    scope: &mut Scope,
    o: &Object,
    name: &str,
) -> Result<(), Error> {
    v.check()?;
    let owner = ref_num(o);
    if scope.wc_values.contains(&owner) {
        return Ok(());
    }
    let value =
        v.x.dereference(o)
            .map_err(|e| e.context(format!("{name} name tree value: dereference")))?
            .clone();
    match &value {
        Object::Dict(_) => validate_web_capture_content_set(v.x, o, &mut scope.wc_content_sets)?,
        Object::Array(a) => {
            for (i, content_set) in a.iter().enumerate() {
                v.check()?;
                validate_web_capture_content_set(v.x, content_set, &mut scope.wc_content_sets)
                    .map_err(|e| e.context(format!("{name} name tree value[{i}]")))?;
            }
        }
        other => {
            return Err(validation(format!(
                "{name} name tree value: expected content set dict or array, got {}",
                type_name(other)
            )));
        }
    }
    if owner > 0 {
        scope.wc_values.insert(owner);
    }
    Ok(())
}

/// `validateIDSNameTreeValue` (14.10.4).
fn validate_ids_name_tree_value(
    v: &mut Validator<'_>,
    scope: &mut Scope,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    v.x.validate_version("IDSNameTreeValue", since)?;
    validate_web_capture_name_tree_value(v, scope, o, "IDS")
}

/// `validateURLSNameTreeValue` (14.10.4).
fn validate_urls_name_tree_value(
    v: &mut Validator<'_>,
    scope: &mut Scope,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    v.x.validate_version("URLSNameTreeValue", since)?;
    validate_web_capture_name_tree_value(v, scope, o, "URLS")
}

/// `hasEmbeddedFileStream`: a file specification dictionary whose `EF` holds a stream.
fn has_embedded_file_stream(x: &mut XRefTable, o: &Object) -> bool {
    if is_url_file_specification(x, o) {
        return false;
    }
    let Object::Dict(d) = o else {
        return false;
    };
    let ef_raw = d.find(b"EF").cloned().unwrap_or(Object::Null);
    let Ok(ef) = x.dereference_dict(&ef_raw) else {
        return false;
    };
    let ef = ef.cloned().unwrap_or_default();
    let keys: [&[u8]; 5] = [b"UF", b"F", b"DOS", b"Mac", b"Unix"];
    for key in keys {
        let raw = ef.find(key).cloned().unwrap_or(Object::Null);
        match x.dereference(&raw) {
            Err(_) => return false,
            Ok(Object::Stream(_)) => return true,
            Ok(_) => {}
        }
    }
    false
}

/// `isEmptyEmbeddedFileSpecification`: a file specification with an empty `EF` dictionary.
fn is_empty_embedded_file_specification(x: &mut XRefTable, o: &Object) -> bool {
    if is_url_file_specification(x, o) {
        return false;
    }
    let Ok(o) = x.dereference(o) else {
        return false;
    };
    let Object::Dict(d) = o else {
        return false;
    };
    let Some(raw_ef) = d.find(b"EF").cloned() else {
        return false;
    };
    matches!(x.dereference_dict(&raw_ef), Ok(Some(ef)) if ef.is_empty())
}

/// `validateEmbeddedFilesNameTreeValue` (7.11.4): a file specification for an embedded file
/// stream. Relaxed validation accepts a null value, an empty `EF` and an `EF` that does not match
/// a file name entry.
fn validate_embedded_files_name_tree_value(x: &mut XRefTable, o: &Object) -> Result<(), Error> {
    // Relaxed: the version check is for 1.1.
    x.validate_version("EmbeddedFilesNameTreeValue", Version::V11)?;

    let null = x
        .dereference(o)
        .map_err(|e| e.context("EmbeddedFiles name tree value: dereference"))?
        .is_null();
    if null {
        return Ok(());
    }
    let f = validate_file_specification_without_link_collection(x, o)
        .map_err(|e| e.context("EmbeddedFiles name tree value"))?;
    if is_embedded_file_specification(x, &f)
        || is_empty_embedded_file_specification(x, &f)
        || has_embedded_file_stream(x, &f)
    {
        return Ok(());
    }
    Err(validation(
        "EmbeddedFiles name tree value: expected matching embedded file stream",
    ))
}

/// `validateRichMediaAssetNameTreeValue`: an embedded file specification.
fn validate_rich_media_asset_name_tree_value(
    x: &mut XRefTable,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    x.validate_version("RichMediaAssetNameTreeValue", since)?;

    let d = x
        .dereference_dict(o)
        .map_err(|e| e.context("RichMedia Assets name tree value: dereference file specification"))?
        .cloned();
    let Some(d) = d else {
        return Err(validation(
            "RichMedia Assets name tree value: missing file specification",
        ));
    };
    if is_url_file_specification(x, &Object::Dict(d.clone())) {
        return Err(validation(
            "RichMedia Assets name tree value: expected embedded file specification",
        ));
    }
    validate_file_specification_without_link_collection(x, o)
        .map_err(|e| e.context("RichMedia Assets name tree value"))?;

    let Some(raw_ef) = d.find(b"EF").cloned() else {
        return Err(validation(
            "RichMedia Assets name tree value: missing EF dictionary",
        ));
    };
    let ef = x
        .dereference_dict(&raw_ef)
        .map_err(|e| e.context("RichMedia Assets name tree value EF"))?
        .cloned();
    let Some(ef) = ef else {
        return Err(validation(
            "RichMedia Assets name tree value: missing EF dictionary",
        ));
    };
    for key in ["F", "UF"] {
        let Some(raw) = ef.find(key.as_bytes()) else {
            continue;
        };
        let embedded_file = x
            .dereference(raw)
            .map_err(|e| e.context(format!("RichMedia Assets name tree value EF.{key}")))?;
        if matches!(embedded_file, Object::Stream(_)) {
            return Ok(());
        }
    }

    Err(validation(
        "RichMedia Assets name tree value: missing embedded file stream reference",
    ))
}

/// `validateSlideShowResource`: a file specification, a stream or another dictionary.
fn validate_slide_show_resource(
    x: &mut XRefTable,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    x.validate_version("SlideShowResource", since)?;

    let resource = x
        .dereference(o)
        .map_err(|e| e.context("slide show resource: dereference"))?
        .clone();
    let d = match &resource {
        Object::Null => return Err(validation("slide show resource: missing object")),
        Object::Dict(d) => d.clone(),
        Object::Stream(sd) => sd.dict.clone(),
        other => {
            return Err(validation(format!(
                "slide show resource: expected dictionary or stream, got {}",
                type_name(other)
            )));
        }
    };

    let typ = validate_name_entry(
        x,
        &d,
        "slideShowResource",
        "Type",
        REQUIRED,
        Version::V10,
        None,
    )?
    .unwrap_or_default();
    // Relaxed accepts `F` for a file specification.
    let is_file_spec = matches!(typ.as_slice(), b"Filespec" | b"FileSpec" | b"F");
    if !is_file_spec {
        return Ok(());
    }

    validate_file_specification_without_link_collection(x, o)
        .map_err(|e| e.context("slide show resource file specification"))?;
    Ok(())
}

/// `validateLegacySlideShowResource`.
fn validate_legacy_slide_show_resource(x: &mut XRefTable, o: &Object) -> Result<(), Error> {
    let resource = x
        .dereference(o)
        .map_err(|e| e.context("slide show resource: dereference"))?;
    let d = match resource {
        Object::Null => return Err(validation("slide show resource: missing object")),
        Object::Dict(d) => d,
        Object::Stream(sd) => &sd.dict,
        _ => return Ok(()),
    };
    if d.find(b"Type").is_none() {
        return Ok(());
    }
    validate_slide_show_resource(x, o, Version::V14)
}

/// `validateLegacySlideShowResources`: a flat array of keys and resource references, accepted in
/// relaxed mode.
fn validate_legacy_slide_show_resources(
    x: &mut XRefTable,
    a: &[Object],
    start_resource: &str,
) -> Result<(), Error> {
    let dict_name = "slideShowDict";
    let entry_name = "Resources";

    validate_array_pairs(a, dict_name, entry_name, 1)?;

    let mut found = false;
    for (n, pair) in a.chunks_exact(2).enumerate() {
        let i = n * 2;
        let [key_raw, resource] = pair else {
            continue;
        };
        let key_obj = x
            .dereference(key_raw)
            .map_err(|e| e.context(format!("{dict_name}.{entry_name}[{i}]: dereference key")))?;
        let key = string_or_hex_literal(key_obj)
            .map_err(|e| e.context(format!("{dict_name}.{entry_name}[{i}]: expected string")))?;
        found = found || key == start_resource;
        if !matches!(resource, Object::Ref(_)) {
            return Err(validation(format!(
                "{dict_name}.{entry_name}[{}]: expected indirect reference",
                i + 1
            )));
        }
        validate_legacy_slide_show_resource(x, resource)
            .map_err(|e| e.context(format!("{dict_name}.{entry_name}[{}]", i + 1)))?;
    }
    if !found {
        return Err(validation(format!(
            "{dict_name}.StartResource: resource {start_resource:?} not found"
        )));
    }
    Ok(())
}

/// Whether the tree `n` has an entry with the decoded key `k` (Go's `Node.Value`, which finds a
/// key wherever it is in the tree).
fn node_has_key(v: &Validator<'_>, n: &Node, k: &str) -> Result<bool, Error> {
    for e in &n.names {
        v.check()?;
        if e.k == k {
            return Ok(true);
        }
    }
    for kid in &n.kids {
        v.check()?;
        if node_has_key(v, kid, k)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `validateSlideShowResources` (13.5): the name tree of the resources, which has to hold the
/// start resource.
fn validate_slide_show_resources(
    v: &mut Validator<'_>,
    d: &Dict,
    start_resource: &str,
) -> Result<(), Error> {
    let dict_name = "slideShowDict";
    let entry_name = "Resources";

    let raw_resources = d.find(entry_name.as_bytes()).cloned();
    let resources = validate_entry(v.x, d, dict_name, entry_name, REQUIRED, Version::V14)?;
    // Relaxed: a flat array is accepted.
    if let Some(Object::Array(a)) = &resources {
        return validate_legacy_slide_show_resources(v.x, a, start_resource);
    }
    let Some(Object::Dict(resources_dict)) = resources else {
        return Err(validation(format!(
            "dict={dict_name} entry={entry_name} invalid type <nil>"
        )));
    };
    let (_, _, tree) = validate_name_tree(
        v,
        "SlideShowResources",
        &resources_dict,
        true,
        raw_resources.as_ref(),
    )?;
    let not_found = || {
        validation(format!(
            "{dict_name}.StartResource: resource {start_resource:?} not found"
        ))
    };
    let Some(tree) = tree else {
        return Err(not_found());
    };
    if !node_has_key(v, &tree, start_resource)? {
        return Err(not_found());
    }
    Ok(())
}

/// `validateSlideShowDict` (13.5, table 297).
fn validate_slide_show_dict(v: &mut Validator<'_>, d: &Dict) -> Result<(), Error> {
    let dict_name = "slideShowDict";
    let v14 = Version::V14;

    // Type, required, name, since V1.4
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Type",
        REQUIRED,
        v14,
        Some(&|s: &[u8]| s == b"SlideShow"),
    )?;

    // Subtype, required, name, since V1.4
    validate_name_entry(
        v.x,
        d,
        dict_name,
        "Subtype",
        REQUIRED,
        v14,
        Some(&|s: &[u8]| s == b"Embedded"),
    )?;

    // StartResource, required, byte string, since V1.4
    let start_resource =
        validate_string_entry(v.x, d, dict_name, "StartResource", REQUIRED, v14, None)?
            .unwrap_or_default();

    // Resources, required name tree, since V1.4
    validate_slide_show_resources(v, d, &start_resource)
}

/// `validateAlternatePresentationsNameTreeValue` (13.5): a slide show dictionary.
fn validate_alternate_presentations_name_tree_value(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    v.x.validate_version("AlternatePresentationsNameTreeValue", since)?;

    let d =
        v.x.dereference_dict(o)
            .map_err(|e| {
                e.context("AlternatePresentations name tree value: dereference slide show dict")
            })?
            .cloned();
    match d {
        Some(d) => validate_slide_show_dict(v, &d),
        None => Ok(()),
    }
}

/// `validateRenditionsNameTreeValue` (13.2.3): a rendition object.
fn validate_renditions_name_tree_value(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    v.x.validate_version("RenditionsNameTreeValue", since)?;

    let d =
        v.x.dereference_dict(o)
            .map_err(|e| e.context("Renditions name tree value: dereference rendition dict"))?
            .cloned();
    match d {
        // Go: validationObjectNumber(0, o), the object number of an indirect value.
        Some(d) => {
            let owner = match o {
                Object::Ref(r) => r.num,
                _ => 0,
            };
            validate_rendition_dict(v, &d, owner, since)
        }
        None => Ok(()),
    }
}

/// `validateIDTreeValueContext`: a structure element dictionary.
fn validate_id_tree_value_context(
    v: &mut Validator<'_>,
    o: &Object,
    since: Version,
) -> Result<(), Error> {
    v.x.validate_version("IDTreeValue", since)?;

    let d =
        v.x.dereference_dict(o)
            .map_err(|e| e.context("IDTree value: dereference structure element dict"))?
            .cloned();
    let Some(d) = d else {
        return Err(validation("IDTree value: missing structure element dict"));
    };

    let (t, _) =
        v.x.dereference_name_entry(&d, b"Type")
            .map_err(|e| e.context("IDTree value Type"))?;
    match t.as_deref() {
        None | Some(b"StructElem") => validate_struct_element_dict_context(v, &d, Some(o), true),
        Some(other) => Err(validation(format!(
            "IDTree value: unexpected dict Type {}, expected StructElem",
            lossy(other)
        ))),
    }
}

/// `validateNameTreeValue`: the value of the tree `name`. The values may be objects of any type;
/// each tree says which it takes.
fn validate_name_tree_value(
    v: &mut Validator<'_>,
    scope: &mut Scope,
    name: &str,
    o: &Object,
) -> Result<(), Error> {
    match name {
        "Dests" => validate_dests_name_tree_value(v.x, o, Version::V12),
        "AP" => validate_ap_name_tree_value(v, o, Version::V13),
        "JavaScript" => validate_java_script_name_tree_value(v, scope, o, Version::V13),
        "3DResources" => validate_3d_resources_name_tree_value(v.x, o, Version::V16),
        "Pages" => validate_pages_name_tree_value(v.x, o, Version::V13),
        "Templates" => validate_templates_name_tree_value(v.x, o, Version::V13),
        "IDS" => validate_ids_name_tree_value(v, scope, o, Version::V13),
        "URLS" => validate_urls_name_tree_value(v, scope, o, Version::V13),
        "EmbeddedFiles" => validate_embedded_files_name_tree_value(v.x, o),
        "RichMediaAssets" => validate_rich_media_asset_name_tree_value(v.x, o, Version::V10),
        "SlideShowResources" => validate_slide_show_resource(v.x, o, Version::V14),
        "AlternatePresentations" => {
            validate_alternate_presentations_name_tree_value(v, o, Version::V14)
        }
        "Renditions" => validate_renditions_name_tree_value(v, o, Version::V15),
        _ => Err(validation(format!("name tree {name}: unknown tree name"))),
    }
}

/// `validateNameTreeValueContext`.
fn validate_name_tree_value_context(
    v: &mut Validator<'_>,
    scope: &mut Scope,
    name: &str,
    o: &Object,
) -> Result<(), Error> {
    if name == "IDTree" {
        return validate_id_tree_value_context(v, o, Version::V13);
    }
    validate_name_tree_value(v, scope, name, o)
}

/// `validateNameTreeKey` (7.9.6): the key `o` as text, as the PDF string bytes it is ordered
/// by, and as the object it dereferences to. The lexical order is not checked (relaxed).
fn validate_name_tree_key(
    x: &mut XRefTable,
    o: &Object,
) -> Result<(String, Vec<u8>, Object), Error> {
    let o = x
        .dereference(o)
        .map_err(|e| e.context("dereference key"))?
        .clone();
    let raw = match &o {
        Object::String(s) => unescape(s),
        Object::Hex(h) => hex_decode(h).map_err(|e| e.context("decode key bytes"))?,
        other => {
            return Err(validation(format!(
                "expected string key, got {}",
                type_name(other)
            )));
        }
    };
    let key = string_or_hex_literal(&o).map_err(|e| e.context("expected string key"))?;
    Ok((key, raw, o))
}

/// Replaces the element `i` of the `/Names` array of the node's dictionary (Go's `a[i] = key`,
/// which the node's dictionary shares with the stored array).
fn set_names_key(node: &mut Node, i: usize, key: &Object) {
    if let Some(Object::Array(a)) = node.d.as_mut().and_then(|d| d.find_mut(b"Names"))
        && let Some(slot) = a.get_mut(i)
    {
        *slot = key.clone();
    }
}

/// The keys and values of a leaf: `validateNameTreeDictNamesEntry` after the lookup of the
/// array. Returns the first and last key as PDF string bytes.
fn validate_name_tree_names(
    v: &mut Validator<'_>,
    scope: &mut Scope,
    name: &str,
    a: &mut [Object],
    node: &mut Node,
) -> Result<(Vec<u8>, Vec<u8>), Error> {
    // arr length needs to be even because of contained key value pairs.
    let mut entries = a.len();
    if !entries.is_multiple_of(2) {
        // Relaxed: a JavaScript tree ignores the last element.
        if name != "JavaScript" {
            return Err(validation(format!(
                "name tree {name} Names: odd entry count {}",
                a.len()
            )));
        }
        entries = entries.saturating_sub(1);
    }

    let mut key = String::new();
    let mut first_key: Option<Vec<u8>> = None;
    let mut last_key = Vec::new();

    for i in 0..entries {
        v.check()?;
        let o = a[i].clone();

        if i.is_multiple_of(2) {
            let (k, raw, key_obj) = validate_name_tree_key(v.x, &o)
                .map_err(|e| e.context(format!("name tree {name} Names[{i}]")))?;
            a[i] = key_obj.clone();
            set_names_key(node, i, &key_obj);

            if first_key.is_none() {
                first_key = Some(raw.clone());
                node.kmin = k.clone();
            }
            last_key = raw;
            node.kmax = k.clone();
            key = k;
            continue;
        }

        validate_name_tree_value_context(v, scope, name, &o)
            .map_err(|e| e.context(format!("name tree {name} key {key:?}")))?;

        node.append_to_names(&key, o);
    }

    Ok((first_key.unwrap_or_default(), last_key))
}

/// `validateNameTreeDictNamesEntry` (7.9.6): a leaf node; `o` is the object the dictionary `d`
/// came from.
fn validate_name_tree_dict_names_entry(
    v: &mut Validator<'_>,
    scope: &mut Scope,
    d: &Dict,
    o: Option<&Object>,
    name: &str,
    node: &mut Node,
) -> Result<(Vec<u8>, Vec<u8>), Error> {
    // Names: array of the form [key1 value1 key2 value2 ... key n value n]
    let mem = v.x.mem().clone();
    let _raw_copy = mem.charge(d.find(b"Names").map_or(0, Object::copy_bytes))?;
    let Some(names_raw) = d.find(b"Names").cloned() else {
        return Err(validation(format!(
            "name tree {name}: missing Kids or Names"
        )));
    };

    let found =
        v.x.dereference_array(&names_raw)
            .map_err(|e| e.context(format!("name tree {name} Names: dereference array")))?;
    // This copy and the one stored back below.
    let _copy = mem.charge(found.map_or(0, |a| 2 * array_copy_bytes(a)))?;
    let a = found.cloned();
    let Some(mut a) = a else {
        return Err(validation(format!("name tree {name}: missing Names array")));
    };
    // The node keeps a copy of the array, and its entries a copy of the keys (decoded and raw)
    // and values in a `Vec` that may be twice their count, for the table's life.
    v.x.held.grow_by(4 * array_copy_bytes(&a))?;
    // Keep the resolved key objects available to the node, which retains their original string
    // bytes.
    if let Some(nd) = node.d.as_mut() {
        nd.set(b"Names", Object::Array(a.clone()));
    }

    let r = validate_name_tree_names(v, scope, name, &mut a, node);

    store_dict_entry(v.x, o, b"Names", Object::Array(a.clone()));
    if let Object::Ref(names_ref) = names_raw {
        store_object(v.x, names_ref, Object::Array(a));
    }
    r
}

fn name_tree_kid_context(name: &str, o: &Object, i: usize) -> String {
    match o {
        Object::Ref(r) => format!("name tree {name} Kids[{i}] obj#{}", r.num),
        _ => format!("name tree {name} Kids[{i}]"),
    }
}

/// `validateNameTreeKids` (7.9.6): the lowest and the highest key of the kids that validated.
/// A kid that fails is skipped unless the error ends the walk.
fn validate_name_tree_kids(
    v: &mut Validator<'_>,
    scope: &mut Scope,
    name: &str,
    a: &[Object],
    node: &mut Node,
    depth: u64,
    visit: &mut TreeVisit,
) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let mut kmin = Vec::new();
    let mut kmax = Vec::new();
    let mut have_kmin = false;

    for (i, o) in a.iter().enumerate() {
        v.check()?;

        let d =
            v.x.dereference_dict(o)
                .map_err(|e| {
                    e.context(format!(
                        "{}: dereference dict",
                        name_tree_kid_context(name, o, i)
                    ))
                })?
                .cloned();
        let Some(d) = d else {
            return Err(validation(format!(
                "{}: missing dict",
                name_tree_kid_context(name, o, i)
            )));
        };

        match validate_name_tree_child(v, scope, name, &d, o, depth.saturating_add(1), visit) {
            Err(e) => {
                let e = wrap_recursion_error(name_tree_kid_context(name, o, i), e);
                if v.check().is_err() || fatal_tree_error(&e) {
                    return Err(e);
                }
            }
            Ok((kmin_kid, kmax_kid, Some(kid))) => {
                kmax = kmax_kid;
                node.kmax.clone_from(&kid.kmax);
                if !have_kmin {
                    kmin = kmin_kid;
                    node.kmin.clone_from(&kid.kmin);
                    have_kmin = true;
                }
                node.kids.push(kid);
            }
            Ok(_) => {}
        }
    }

    Ok((kmin, kmax))
}

/// `validateNameTreeDictLimitsEntry`: the `/Limits` of a non-root node are required and have to
/// be two strings. Relaxed validation digests limits that differ from the keys.
fn validate_name_tree_dict_limits_entry(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let a = validate_string_array_entry(
        x,
        d,
        "nameTreeDict",
        "Limits",
        REQUIRED,
        Version::V10,
        Some(&|a: &[Object]| a.len() == 2),
    )?
    .unwrap_or_default();

    for (i, e) in a.iter().enumerate() {
        let o = x
            .dereference(e)
            .map_err(|err| err.context(format!("name tree Limits[{i}]: dereference")))?
            .clone();
        validate_name_tree_key(x, &o)
            .map_err(|err| err.context(format!("name tree Limits[{i}]: expected string")))?;
    }
    Ok(())
}

/// `validateNameTreeChild`.
fn validate_name_tree_child(
    v: &mut Validator<'_>,
    scope: &mut Scope,
    name: &str,
    d: &Dict,
    o: &Object,
    depth: u64,
    visit: &mut TreeVisit,
) -> Result<(Vec<u8>, Vec<u8>, Option<Node>), Error> {
    let n = visit.enter(o)?;
    let r = validate_name_tree_depth(v, scope, name, d, Some(o), false, depth, visit);
    visit.leave(n);
    r
}

/// `validateNameTreeDepthWithViolations` (7.7.4, 7.9.6): a node has a `/Kids` or a `/Names`
/// entry. Returns the lowest and highest key as PDF string bytes and the node; no node for an
/// empty `/Kids`.
fn validate_name_tree_depth(
    v: &mut Validator<'_>,
    scope: &mut Scope,
    name: &str,
    d: &Dict,
    o: Option<&Object>,
    root: bool,
    depth: u64,
    visit: &mut TreeVisit,
) -> Result<(Vec<u8>, Vec<u8>, Option<Node>), Error> {
    check_validation_tree(v, &format!("name tree {name}"), depth)?;

    // The node keeps a copy of its dictionary for the table's life.
    v.x.held.grow_by(dict_copy_bytes(d))?;
    let mut node = Node::new(d.clone());

    // Kids: array of indirect references to the immediate children of this node.
    let (kmin, kmax) = if let Some(kids) = d.find(b"Kids") {
        // Intermediate node
        let a =
            v.x.dereference_array(kids)
                .map_err(|e| e.context(format!("name tree {name} Kids: dereference array")))?
                .cloned();
        let Some(a) = a else {
            return Err(validation(format!("name tree {name}: missing Kids array")));
        };

        // Relaxed: an empty array is skipped.
        if a.is_empty() {
            return Ok((Vec::new(), Vec::new(), None));
        }

        validate_name_tree_kids(v, scope, name, &a, &mut node, depth, visit)?
    } else {
        // Leaf node; relaxed validation accepts a root with neither Kids nor Names.
        if root && d.find(b"Names").is_none() {
            return Ok((Vec::new(), Vec::new(), Some(node)));
        }
        validate_name_tree_dict_names_entry(v, scope, d, o, name, &mut node)?
    };

    if !root {
        validate_name_tree_dict_limits_entry(v.x, d)
            .map_err(|e| e.context(format!("name tree {name} Limits")))?;
    }

    Ok((kmin, kmax, Some(node)))
}

/// `validateNameTree` (nameTree.go:1194): validates the name tree `name` with the root node `d`
/// (`root_object` is the object it came from) and returns its lowest and highest key and the
/// internalised tree.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_name_tree(
    v: &mut Validator<'_>,
    name: &str,
    d: &Dict,
    root: bool,
    root_object: Option<&Object>,
) -> Result<(String, String, Option<Node>), Error> {
    let mut scope = Scope::default();
    let mut visit = TreeVisit::new(true, root_object);
    let (kmin, kmax, node) =
        validate_name_tree_depth(v, &mut scope, name, d, root_object, root, 0, &mut visit)?;
    Ok((lossy(&kmin), lossy(&kmax), node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::error::Sentinel;
    use crate::object::Ref;
    use std::sync::atomic::AtomicBool;

    fn table(version: Version) -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(version);
        x
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn name(s: &str) -> Object {
        Object::Name(s.as_bytes().to_vec())
    }

    fn string(s: &str) -> Object {
        Object::String(s.as_bytes().to_vec())
    }

    fn r(num: i64) -> Object {
        Object::Ref(Ref { num, generation: 0 })
    }

    fn page() -> Object {
        Object::Dict(dict(&[("Type", name("Page"))]))
    }

    fn pairs(keys: &[Object], value: &Object) -> Object {
        let mut a = Vec::new();
        for k in keys {
            a.push(k.clone());
            a.push(value.clone());
        }
        Object::Array(a)
    }

    // Port of the relaxed cases of TestTreeFirstKeyRanges (name trees): a valid empty first key
    // stays the lowest key, in leaf and intermediate ranges.
    #[test]
    fn the_first_key_of_a_tree_may_be_empty() {
        for version in [Version::V17, Version::V20] {
            let mut x = table(version);
            let keys = [string(""), string("a"), string("b"), string("c")];
            let leaf = dict(&[
                ("Names", pairs(&keys[..2], &page())),
                (
                    "Limits",
                    Object::Array(vec![keys[0].clone(), keys[1].clone()]),
                ),
            ]);
            x.ind_ref_for_object(10, Object::Dict(leaf));
            let leaf2 = dict(&[
                ("Names", pairs(&keys[2..], &page())),
                (
                    "Limits",
                    Object::Array(vec![keys[2].clone(), keys[3].clone()]),
                ),
            ]);
            x.ind_ref_for_object(11, Object::Dict(leaf2));
            let root = dict(&[("Kids", Object::Array(vec![r(10), r(11)]))]);
            let cancel = AtomicBool::new(false);
            let mut v = Validator::new(&mut x, &cancel);
            let (first, last, node) =
                validate_name_tree(&mut v, "Pages", &root, true, None).unwrap();
            let node = node.unwrap();
            assert_eq!((first.as_str(), last.as_str()), ("", "c"));
            assert_eq!((node.kmin.as_str(), node.kmax.as_str()), ("", "c"));
            assert_eq!(node.kids.len(), 2);
            assert!(node_has_key(&v, &node, "").unwrap());
            assert!(node_has_key(&v, &node, "c").unwrap());
            assert!(!node_has_key(&v, &node, "d").unwrap());
        }
    }

    // Port of the relaxed cases of TestTreeKeyOrder: a tree out of order, with duplicates or
    // with keys of mixed representation is accepted.
    #[test]
    fn relaxed_validation_accepts_unordered_keys() {
        let cases: [Vec<Object>; 6] = [
            vec![string("z"), string("a")],
            vec![string("aa"), string("a")],
            vec![string("a"), string("")],
            vec![string("a"), string("a")],
            vec![string("a"), Object::Hex(b"62".to_vec())],
            vec![Object::Hex(b"81".to_vec()), Object::Hex(b"80".to_vec())],
        ];
        for keys in cases {
            let mut x = table(Version::V17);
            let root = dict(&[("Names", pairs(&keys, &page()))]);
            let cancel = AtomicBool::new(false);
            let mut v = Validator::new(&mut x, &cancel);
            assert!(validate_name_tree(&mut v, "Pages", &root, true, None).is_ok());
        }
    }

    #[test]
    fn a_node_keeps_the_resolved_keys_and_the_stored_dictionary_is_updated() {
        let mut x = table(Version::V17);
        x.ind_ref_for_object(20, string("k"));
        let names = Object::Array(vec![r(20), page()]);
        x.ind_ref_for_object(21, names);
        let root = dict(&[("Names", r(21))]);
        x.ind_ref_for_object(22, Object::Dict(root.clone()));
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let rr = Ref {
            num: 22,
            generation: 0,
        };
        let (_, _, node) =
            validate_name_tree(&mut v, "Pages", &root, true, Some(&Object::Ref(rr))).unwrap();
        let node = node.unwrap();
        assert_eq!(node.names.len(), 1);
        assert_eq!(node.names[0].k, "k");
        assert_eq!(node.names[0].raw.as_deref(), Some(b"k".as_slice()));
        // The reference to the key is replaced by the key in the stored array, which the stored
        // node dictionary holds directly.
        let stored =
            v.x.dereference_dict(&Object::Ref(rr))
                .unwrap()
                .cloned()
                .unwrap();
        let Some(Object::Array(a)) = stored.find(b"Names") else {
            panic!("Names is not an array");
        };
        assert!(matches!(&a[0], Object::String(s) if s == b"k"));
    }

    #[test]
    fn a_root_without_kids_or_names_is_accepted_and_a_kid_without_them_is_skipped() {
        let mut x = table(Version::V17);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let (_, _, node) =
            validate_name_tree(&mut v, "Pages", &Dict::default(), true, None).unwrap();
        assert!(node.unwrap().names.is_empty());
        // A leaf that is not the root needs its Names.
        assert!(validate_name_tree(&mut v, "Pages", &Dict::default(), false, None).is_err());
        // A kid without Names fails, and is skipped.
        v.x.ind_ref_for_object(30, Object::Dict(Dict::default()));
        let root = dict(&[("Kids", Object::Array(vec![r(30)]))]);
        let (_, _, node) = validate_name_tree(&mut v, "Pages", &root, true, None).unwrap();
        assert!(node.unwrap().kids.is_empty());
    }

    #[test]
    fn a_duplicate_kid_ends_the_walk() {
        let mut x = table(Version::V17);
        let keys = [string("a")];
        let leaf = dict(&[
            ("Names", pairs(&keys, &page())),
            (
                "Limits",
                Object::Array(vec![keys[0].clone(), keys[0].clone()]),
            ),
        ]);
        x.ind_ref_for_object(10, Object::Dict(leaf));
        let root = dict(&[("Kids", Object::Array(vec![r(10), r(10)]))]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let err = validate_name_tree(&mut v, "Pages", &root, true, None).unwrap_err();
        assert!(err.is(Sentinel::NameTreeDuplicate), "{err}");
    }

    #[test]
    fn an_unknown_tree_and_a_bad_key_are_errors() {
        let mut x = table(Version::V17);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let root = dict(&[("Names", pairs(&[string("a")], &page()))]);
        let err = validate_name_tree(&mut v, "Nope", &root, true, None).unwrap_err();
        assert!(err.to_string().contains("unknown tree name"), "{err}");
        let bad = dict(&[("Names", pairs(&[Object::Integer(1)], &page()))]);
        let err = validate_name_tree(&mut v, "Pages", &bad, true, None).unwrap_err();
        assert!(err.to_string().contains("expected string key"), "{err}");
        // An odd array is an error, except for a JavaScript tree.
        let odd = dict(&[("Names", Object::Array(vec![string("a")]))]);
        assert!(validate_name_tree(&mut v, "Pages", &odd, true, None).is_err());
    }

    #[test]
    fn a_page_value_needs_its_type() {
        let mut x = table(Version::V17);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        let wrong = Object::Dict(dict(&[("Type", name("Pages"))]));
        let root = dict(&[("Names", pairs(&[string("a")], &wrong))]);
        assert!(validate_name_tree(&mut v, "Pages", &root, true, None).is_err());
    }

    #[test]
    fn a_cancelled_walk_stops() {
        let mut x = table(Version::V17);
        let cancel = AtomicBool::new(true);
        let mut v = Validator::new(&mut x, &cancel);
        let root = dict(&[("Names", pairs(&[string("a")], &page()))]);
        let err = validate_name_tree(&mut v, "Pages", &root, true, None).unwrap_err();
        assert_eq!(err, Error::Cancelled);
    }

    #[test]
    fn a_capture_command_needs_a_url() {
        let mut x = table(Version::V17);
        assert!(
            validate_capture_command_dict(&mut x, &dict(&[("L", Object::Integer(1))])).is_err()
        );
        let ok = dict(&[
            ("URL", string("http://example.com")),
            ("L", Object::Integer(1)),
        ]);
        assert!(validate_capture_command_dict(&mut x, &ok).is_ok());
    }

    // Port of TestStructureTraversalCancellation, IDTree entry: a cancelled walk stops at the
    // first check.
    #[test]
    fn the_id_tree_value_walk_honours_cancellation() {
        let mut x = table(Version::V17);
        let elem = dict(&[("Type", name("StructElem")), ("S", name("Div"))]);
        x.ind_ref_for_object(5, Object::Dict(elem));
        let cancel = AtomicBool::new(true);
        let mut v = Validator::new(&mut x, &cancel);
        let mut scope = Scope::default();
        let err =
            validate_name_tree_value_context(&mut v, &mut scope, "IDTree", &r(5)).unwrap_err();
        assert_eq!(err, Error::Cancelled);
    }
}
