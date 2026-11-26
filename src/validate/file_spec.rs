//! File specifications (Go: validate/fileSpec.go and the two predicates of link_collect.go).
//!
//! Relaxed validation only. Not ported: the link collection (`collectFileSpecificationTarget`,
//! `collectLinkTarget`; `ValidateLinks` is false), so `validateFileSpecification` and
//! `validateFileSpecificationWithoutLinkCollection` do the same; the validation notices;
//! `validateMediaClipDataType`, whose failure only yields a notice; and the owner object numbers.
//!
//! [`is_url_file_specification`] and [`is_embedded_file_specification`] are the predicates of
//! link_collect.go that the design lists. Nothing in this package calls them, because every use
//! in Go only decides what to collect.
//!
//! `validateURLString` is `url.ParseRequestURI` of Go 1.27 (net/url/url.go, BSD licence, see
//! NOTICE), reduced to its verdict: [`request_uri_is_valid`].
//!
//! # Calls into other packages
//!
//! None: [`validate_file_specification_or_form_object`] calls `xobject::validate_xobject_type` and
//! `xobject::validate_form_stream_dict` (WP 19).

use super::Validator;
use super::object::{
    OPTIONAL, REQUIRED, dict_entry_context, object_context, validate_boolean_entry,
    validate_date_entry, validate_dict_entry, validate_entry, validate_integer_entry,
    validate_name_entry, validate_stream_dict_entry, validate_stream_dict_for_object,
    validate_string_array_entry, validate_string_entry,
};
use crate::error::Error;
use crate::model::deref::type_name;
use crate::model::{Version, XRefTable};
use crate::object::{Dict, Object, StreamDict};

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

// ---------------------------------------------------------------------------------------------
// url.ParseRequestURI

/// The parts of a URL that `unescape` treats differently (net/url `encoding`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    Host,
    Zone,
    Path,
    UserPassword,
}

/// `shouldEscape(c, encodeHost)`, which is the same for the zone.
fn host_byte_escapes(c: u8) -> bool {
    !(c.is_ascii_alphanumeric() || b"!$&'()*+,;=:[]<>\"-_.~".contains(&c))
}

fn unhex(c: u8) -> u8 {
    (c as char).to_digit(16).map_or(0, |d| d as u8)
}

/// `unescape`: whether every `%` of `s` is followed by two hexadecimal digits, and, for a host or
/// zone, whether its bytes may appear there.
fn unescape_ok(s: &[u8], mode: Escape) -> bool {
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'%' {
            if i + 2 >= s.len() || !s[i + 1].is_ascii_hexdigit() || !s[i + 2].is_ascii_hexdigit() {
                return false;
            }
            let escaped_percent = &s[i..i + 3] == b"%25";
            if mode == Escape::Host && unhex(s[i + 1]) < 8 && !escaped_percent {
                return false;
            }
            if mode == Escape::Zone {
                let v = (unhex(s[i + 1]) << 4) | unhex(s[i + 2]);
                if !escaped_percent && v != b' ' && host_byte_escapes(v) {
                    return false;
                }
            }
            i += 3;
        } else {
            if matches!(mode, Escape::Host | Escape::Zone) && c < 0x80 && host_byte_escapes(c) {
                return false;
            }
            i += 1;
        }
    }
    true
}

/// The bytes of `s`, which `unescape_ok` accepted, with its escapes decoded.
fn percent_decode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' && i + 2 < s.len() {
            out.push((unhex(s[i + 1]) << 4) | unhex(s[i + 2]));
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

/// `getScheme`: the scheme and the rest of `raw`; `None` for "missing protocol scheme".
fn get_scheme(raw: &str) -> Option<(&str, &str)> {
    for (i, c) in raw.bytes().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' => {
                if i == 0 {
                    return Some(("", raw));
                }
            }
            b':' => {
                if i == 0 {
                    return None;
                }
                return Some((&raw[..i], &raw[i + 1..]));
            }
            _ => return Some(("", raw)),
        }
    }
    Some(("", raw))
}

/// `validOptionalPort`.
fn valid_optional_port(port: &str) -> bool {
    match port.strip_prefix(':') {
        None => port.is_empty(),
        Some(digits) => digits.bytes().all(|b| b.is_ascii_digit()),
    }
}

/// `validUserinfo`.
fn valid_userinfo(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._:~!$&'()*+,;=%@".contains(&b))
}

/// The IPv4 part of an IPv6 address (`parseIPv4Fields`).
fn ipv4_fields_valid(s: &[u8]) -> bool {
    let (mut val, mut pos, mut dig_len) = (0u32, 0usize, 0usize);
    for (i, &c) in s.iter().enumerate() {
        if c.is_ascii_digit() {
            if dig_len == 1 && val == 0 {
                return false;
            }
            val = val * 10 + u32::from(c - b'0');
            dig_len += 1;
            if val > 255 {
                return false;
            }
        } else if c == b'.' {
            if i == 0 || i == s.len() - 1 || s[i - 1] == b'.' || pos == 3 {
                return false;
            }
            pos += 1;
            val = 0;
            dig_len = 0;
        } else {
            return false;
        }
    }
    pos >= 3
}

/// `netip.parseIPv6`: whether `input` is an IPv6 address, with an optional zone.
fn parse_ipv6(input: &[u8]) -> bool {
    let mut s = input;
    if let Some(i) = s.iter().position(|&c| c == b'%') {
        if i + 1 == s.len() {
            // An explicit zone must not be empty.
            return false;
        }
        s = &s[..i];
    }
    let mut ellipsis: Option<usize> = None;
    if s.starts_with(b"::") {
        ellipsis = Some(0);
        s = &s[2..];
        if s.is_empty() {
            return true;
        }
    }
    let mut i = 0usize;
    while i < 16 {
        let mut off = 0;
        let mut acc = 0u32;
        while off < s.len() {
            let Some(digit) = (s[off] as char).to_digit(16) else {
                break;
            };
            acc = (acc << 4) + digit;
            if off > 3 || acc > 0xffff {
                return false;
            }
            off += 1;
        }
        if off == 0 {
            return false;
        }
        if off < s.len() && s[off] == b'.' {
            if (ellipsis.is_none() && i != 12) || i + 4 > 16 || !ipv4_fields_valid(s) {
                return false;
            }
            s = b"";
            i += 4;
            break;
        }
        i += 2;
        s = &s[off..];
        if s.is_empty() {
            break;
        }
        if s[0] != b':' || s.len() == 1 {
            return false;
        }
        s = &s[1..];
        if s[0] == b':' {
            if ellipsis.is_some() {
                return false;
            }
            ellipsis = Some(i);
            s = &s[1..];
            if s.is_empty() {
                break;
            }
        }
    }
    if !s.is_empty() {
        return false;
    }
    if i < 16 {
        ellipsis.is_some()
    } else {
        ellipsis.is_none()
    }
}

/// `netip.ParseAddr` for the host of an IP literal: only an IPv6 address may be in brackets.
fn ipv6_literal_is_valid(s: &[u8]) -> bool {
    match s.iter().find(|&&c| matches!(c, b'.' | b':' | b'%')) {
        Some(b':') => parse_ipv6(s),
        _ => false,
    }
}

/// The IP-literal branch of `parseHost`; `host` starts with `[`.
fn bracketed_host_is_valid(host: &str) -> bool {
    let Some(close) = host.rfind(']') else {
        return false;
    };
    if !valid_optional_port(&host[close + 1..]) {
        return false;
    }
    let hostname = &host[1..close];
    let decoded = match hostname.find("%25") {
        Some(z) => {
            let (name, zone) = hostname.as_bytes().split_at(z);
            if !unescape_ok(name, Escape::Host) || !unescape_ok(zone, Escape::Zone) {
                return false;
            }
            let mut d = percent_decode(name);
            d.extend(percent_decode(zone));
            d
        }
        None => {
            if !unescape_ok(hostname.as_bytes(), Escape::Host) {
                return false;
            }
            percent_decode(hostname.as_bytes())
        }
    };
    ipv6_literal_is_valid(&decoded)
}

/// `parseHost`. Only http and https URLs may not have more than one colon in the host (the
/// default of `GODEBUG=urlstrictcolons`).
fn host_is_valid(scheme: &str, host: &str) -> bool {
    if let Some(open) = host.rfind('[') {
        return open == 0 && bracketed_host_is_valid(host);
    }
    if let Some(first) = host.find(':') {
        let last = host.rfind(':').unwrap_or(first);
        let i = if last != first && scheme != "http" && scheme != "https" {
            last
        } else {
            first
        };
        if !valid_optional_port(&host[i..]) {
            return false;
        }
    }
    unescape_ok(host.as_bytes(), Escape::Host)
}

/// `parseAuthority`.
fn authority_is_valid(scheme: &str, authority: &str) -> bool {
    let Some(at) = authority.rfind('@') else {
        return host_is_valid(scheme, authority);
    };
    if !host_is_valid(scheme, &authority[at + 1..]) {
        return false;
    }
    let userinfo = &authority[..at];
    if !valid_userinfo(userinfo) {
        return false;
    }
    match userinfo.split_once(':') {
        None => unescape_ok(userinfo.as_bytes(), Escape::UserPassword),
        Some((user, password)) => {
            unescape_ok(user.as_bytes(), Escape::UserPassword)
                && unescape_ok(password.as_bytes(), Escape::UserPassword)
        }
    }
}

/// `url.ParseRequestURI(raw)` succeeds: `raw` is an absolute URI or an absolute path.
fn request_uri_is_valid(raw: &str) -> bool {
    if raw.bytes().any(|c| c < b' ' || c == 0x7f) || raw.is_empty() {
        return false;
    }
    if raw == "*" {
        return true;
    }
    let Some((scheme, mut rest)) = get_scheme(raw) else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    if rest.ends_with('?') && rest.matches('?').count() == 1 {
        rest = &rest[..rest.len() - 1];
    } else if let Some((before, _)) = rest.split_once('?') {
        rest = before;
    }
    if !rest.starts_with('/') {
        // A rootless path of a URI is opaque; there is no relative reference in a request.
        return !scheme.is_empty();
    }
    if !scheme.is_empty()
        && let Some(after) = rest.strip_prefix("//")
    {
        let (authority, path) = match after.find('/') {
            Some(i) => after.split_at(i),
            None => (after, ""),
        };
        if !authority_is_valid(&scheme, authority) {
            return false;
        }
        rest = path;
    }
    unescape_ok(rest.as_bytes(), Escape::Path)
}

/// `validateURLString` (RFC 1738, see 7.11.5).
fn validate_url_string(s: &str) -> bool {
    request_uri_is_valid(s)
}

// ---------------------------------------------------------------------------------------------
// Embedded files

/// `validateEmbeddedFileStreamMacParameterDict`.
fn validate_embedded_file_stream_mac_parameter_dict(
    x: &mut XRefTable,
    d: &Dict,
) -> Result<(), Error> {
    let dict_name = "embeddedFileStreamMacParameterDict";
    // The file type and creator integers of Mac OS, and the binary contents of the resource fork.
    validate_integer_entry(x, d, dict_name, "Subtype", OPTIONAL, Version::V10, None)?;
    validate_integer_entry(x, d, dict_name, "Creator", OPTIONAL, Version::V10, None)?;
    validate_stream_dict_entry(x, d, dict_name, "ResFork", OPTIONAL, Version::V10, None)?;
    Ok(())
}

/// `validateEmbeddedFileStreamParameterDict`.
fn validate_embedded_file_stream_parameter_dict(
    x: &mut XRefTable,
    o: &Object,
) -> Result<(), Error> {
    let context = object_context("embedded file stream Params", Some(o));
    let d = x
        .dereference_dict(o)
        .map_err(|e| e.context(format!("{context}: dereference dict")))?
        .cloned();
    let Some(d) = d else {
        return Ok(());
    };
    let dict_name = "embeddedFileStreamParmDict";
    validate_integer_entry(x, &d, dict_name, "Size", OPTIONAL, Version::V10, None)?;
    validate_date_entry(x, &d, dict_name, "CreationDate", OPTIONAL, Version::V10)?;
    validate_date_entry(x, &d, dict_name, "ModDate", OPTIONAL, Version::V10)?;
    let mac = validate_dict_entry(x, &d, dict_name, "Mac", OPTIONAL, Version::V10, None)?;
    if let Some(mac) = mac {
        validate_embedded_file_stream_mac_parameter_dict(x, &mac)?;
    }
    validate_string_entry(x, &d, dict_name, "CheckSum", OPTIONAL, Version::V10, None)?;
    Ok(())
}

/// `validateEmbeddedFileStreamDict`.
fn validate_embedded_file_stream_dict(x: &mut XRefTable, sd: &StreamDict) -> Result<(), Error> {
    let dict_name = "embeddedFileStreamDict";
    validate_name_entry(
        x,
        &sd.dict,
        dict_name,
        "Type",
        OPTIONAL,
        Version::V10,
        Some(&|s: &[u8]| s == b"EmbeddedFile"),
    )?;
    validate_name_entry(
        x,
        &sd.dict,
        dict_name,
        "Subtype",
        OPTIONAL,
        Version::V10,
        None,
    )?;
    // Params, optional, parameter dict with additional file-specific information.
    if let Some(o) = sd.dict.find(b"Params").filter(|o| !o.is_null()) {
        validate_embedded_file_stream_parameter_dict(x, o)
            .map_err(|e| e.context("embedded file stream dict Params"))?;
    }
    Ok(())
}

/// `validateFileSpecDictEntryEFDict`: the values of `F` and `UF` are embedded file streams.
fn validate_file_spec_dict_entry_ef_dict(x: &mut XRefTable, ef: &Dict) -> Result<(), Error> {
    for (k, obj) in ef.iter() {
        let key = lossy(k);
        if !matches!(
            k.as_slice(),
            b"F" | b"UF" | b"DOS" | b"Mac" | b"Unix" | b"Subtype"
        ) {
            return Err(Error::Validation(format!(
                "embedded file dict: invalid key {key}"
            )));
        }
        if matches!(k.as_slice(), b"F" | b"UF") {
            let context = object_context(&format!("fileSpec.EF.{key}"), Some(obj));
            let sd =
                validate_stream_dict_for_object(x, obj).map_err(|e| e.context(context.clone()))?;
            validate_embedded_file_stream_dict(x, &sd).map_err(|e| e.context(context))?;
        }
    }
    Ok(())
}

/// `validateRFDictFilesArray`: pairs of an embedded file stream and a description string.
fn validate_rf_dict_files_array(x: &mut XRefTable, a: &[Object]) -> Result<(), Error> {
    if !a.len().is_multiple_of(2) {
        return Err(Error::Validation(
            "related files array: corrupt length".to_string(),
        ));
    }
    for (k, v) in a.iter().enumerate() {
        let missing = || Error::Validation(format!("related files array[{k}]: missing entry"));
        if v.is_null() {
            return Err(missing());
        }
        let is_string = match x.dereference(v) {
            Err(e) => return Err(e.context(format!("related files array[{k}]: dereference"))),
            Ok(Object::Null) => return Err(missing()),
            Ok(o) => matches!(o, Object::String(_) | Object::Hex(_)),
        };
        if k % 2 > 0 {
            if !is_string {
                return Err(Error::Validation(format!(
                    "related files array[{k}]: expected description string"
                )));
            }
        } else {
            let sd = validate_stream_dict_for_object(x, v).map_err(|e| {
                e.context(format!("related files array[{k}]: embedded file stream"))
            })?;
            validate_embedded_file_stream_dict(x, &sd).map_err(|e| {
                e.context(format!(
                    "related files array[{k}]: embedded file stream dict"
                ))
            })?;
        }
    }
    Ok(())
}

/// `validateFileSpecDictEntriesEFAndRF`: `EF` only, or `EF` and `RF`.
fn validate_file_spec_dict_entries_ef_and_rf(
    x: &mut XRefTable,
    ef: &Dict,
    rf: Option<&Dict>,
) -> Result<(), Error> {
    validate_file_spec_dict_entry_ef_dict(x, ef)?;
    let Some(rf) = rf else {
        return Ok(());
    };
    for (k, val) in rf.iter() {
        let key = lossy(k);
        if ef.find(k).is_none() {
            return Err(Error::Validation(format!(
                "related files dict: entry {key} missing corresponding EF entry"
            )));
        }
        // The value is a related files array, see 7.11.4.2.
        let a = x
            .dereference_array(val)
            .map_err(|e| {
                e.context(format!(
                    "related files dict: entry {key}: dereference array"
                ))
            })?
            .cloned();
        let Some(a) = a else {
            continue;
        };
        validate_rf_dict_files_array(x, &a)
            .map_err(|e| e.context(format!("related files dict: entry {key}")))?;
    }
    Ok(())
}

/// `validateFileSpecDictEFAndRF`: `RF` since 1.3, `EF` (required if `RF` is present) and the
/// `Type` that `EF`, `EP` or `RF` make required.
fn validate_file_spec_dict_ef_and_rf(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    has_ep: bool,
) -> Result<(), Error> {
    let rf = validate_dict_entry(x, d, dict_name, "RF", OPTIONAL, Version::V13, None)?;
    let ef = validate_dict_entry(x, d, dict_name, "EF", rf.is_some(), Version::V11, None)?;
    let required = rf.is_some() || ef.is_some() || has_ep;
    validate_name_entry(
        x,
        d,
        dict_name,
        "Type",
        required,
        Version::V10,
        Some(&|s: &[u8]| s == b"Filespec" || s == b"FileSpec" || s == b"F"),
    )?;
    if let Some(ef) = ef {
        validate_file_spec_dict_entries_ef_and_rf(x, &ef, rf.as_ref())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// File specification dictionaries

/// `validateFileSpecDictPart1`: `FS`, `UF` and `F`. A file specification string always passes
/// (`validateFileSpecString`), so only a URL is checked.
fn validate_file_spec_dict_part1(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    let fs = validate_name_entry(x, d, dict_name, "FS", OPTIONAL, Version::V10, None)?;
    let uf = validate_string_entry(x, d, dict_name, "UF", OPTIONAL, Version::V13, None)?;

    // DOS, Mac and Unix, byte strings, optional, obsolescent.
    let obsolete: [&[u8]; 3] = [b"DOS", b"Mac", b"Unix"];
    let any_obsolete = obsolete.iter().any(|k| d.find(k).is_some());

    let validate: Option<&dyn Fn(&str) -> bool> = if fs.as_deref() == Some(b"URL".as_slice()) {
        Some(&validate_url_string)
    } else {
        None
    };
    let required = !any_obsolete && uf.is_none();
    validate_string_entry(x, d, dict_name, "F", required, Version::V10, validate)?;
    Ok(())
}

/// `isSecondClassName` (Annex E.2, including the 2020 prefix clarification).
fn is_second_class_name(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 4 || b[..4].iter().any(|&c| c > 127) {
        return false;
    }
    b[..b.len().min(5)].iter().any(|&c| c == b'_' || c == b':')
}

/// `validateFileSpecDictPart2` (7.11.3).
fn validate_file_spec_dict_part2(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
) -> Result<(), Error> {
    // ID, optional, array of two strings.
    validate_string_array_entry(
        x,
        d,
        dict_name,
        "ID",
        OPTIONAL,
        Version::V11,
        Some(&|a: &[Object]| a.len() == 2),
    )?;
    validate_boolean_entry(x, d, dict_name, "V", OPTIONAL, Version::V12, None)?;
    // EP, optional, encrypted payload dict, since 2.0.
    let ep = validate_dict_entry(x, d, dict_name, "EP", OPTIONAL, Version::V20, None)?;
    let has_ep = ep.is_some_and(|ep| !ep.is_empty());
    validate_file_spec_dict_ef_and_rf(x, d, dict_name, has_ep)?;
    validate_string_entry(x, d, dict_name, "Desc", OPTIONAL, Version::V10, None)?;
    // CI, optional, collection item dict.
    validate_dict_entry(x, d, dict_name, "CI", OPTIONAL, Version::V17, None)?;
    // Thumb, optional, thumbnail image.
    validate_stream_dict_entry(x, d, dict_name, "Thumb", OPTIONAL, Version::V14, None)?;
    // AFRelationship, optional, associated file semantics.
    let pdf20 = x.pdf20()?;
    validate_name_entry(
        x,
        d,
        dict_name,
        "AFRelationship",
        OPTIONAL,
        Version::V14,
        Some(&|s: &[u8]| {
            (pdf20 && is_second_class_name(&lossy(s)))
                || matches!(
                    s,
                    b"Source"
                        | b"Data"
                        | b"Alternative"
                        | b"Supplement"
                        | b"EncryptedPayload"
                        | b"FormData"
                        | b"Schema"
                        | b"Unspecified"
                )
        }),
    )?;
    Ok(())
}

/// `validateFileSpecDict` (7.11.3).
fn validate_file_spec_dict(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let dict_name = "fileSpecDict";
    validate_file_spec_dict_part1(x, d, dict_name)?;
    validate_file_spec_dict_part2(x, d, dict_name)
}

/// `validateFileSpecification` (7.11): the file specification `o` is or refers to, dereferenced.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_file_specification(x: &mut XRefTable, o: &Object) -> Result<Object, Error> {
    let context = object_context("file specification", Some(o));
    let target = x
        .dereference(o)
        .map_err(|e| e.context(format!("{context}: dereference")))?
        .clone();
    match &target {
        Object::String(_) | Object::Hex(_) => {}
        Object::Dict(d) => {
            validate_file_spec_dict(x, d).map_err(|e| e.context(format!("{context} dict")))?;
        }
        other => {
            return Err(Error::Validation(format!(
                "{context}: invalid type {}",
                type_name(other)
            )));
        }
    }
    Ok(target)
}

/// `validateFileSpecificationWithoutLinkCollection`: [`validate_file_specification`], since the
/// link collection is not ported.
///
/// # Errors
/// As [`validate_file_specification`].
pub fn validate_file_specification_without_link_collection(
    x: &mut XRefTable,
    o: &Object,
) -> Result<Object, Error> {
    validate_file_specification(x, o)
}

/// `validateURLSpecification` (7.11.4): a dictionary with the file system `URL` and a URL.
/// Returns `o` itself.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_url_specification(x: &mut XRefTable, o: &Object) -> Result<Object, Error> {
    let context = object_context("URL specification", Some(o));
    let d = x
        .dereference_dict(o)
        .map_err(|e| e.context(format!("{context}: dereference dict")))?
        .cloned();
    let Some(d) = d else {
        return Err(Error::Validation(format!("{context}: missing dict")));
    };
    let dict_name = "urlSpec";
    // FS, required, name.
    validate_name_entry(
        x,
        &d,
        dict_name,
        "FS",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"URL"),
    )
    .map_err(|e| e.context(format!("{context} FS")))?;
    // F, required, string, URL (Internet RFC 1738).
    validate_string_entry(
        x,
        &d,
        dict_name,
        "F",
        REQUIRED,
        Version::V10,
        Some(&validate_url_string),
    )
    .map_err(|e| e.context(format!("{context} F")))?;
    Ok(o.clone())
}

/// `validateFileSpecEntry`: the file specification entry `entry_name`, dereferenced; `None` if it
/// is absent or null.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups, with the entry as context.
pub fn validate_file_spec_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<Option<Object>, Error> {
    let raw = d.find(entry_name.as_bytes()).cloned();
    let context = dict_entry_context(dict_name, entry_name, raw.as_ref());
    let o = validate_entry(x, d, dict_name, entry_name, required, since)
        .map_err(|e| e.context(context.clone()))?;
    if o.is_none() {
        return Ok(None);
    }
    x.validate_version("fileSpec", since)
        .map_err(|e| e.context(context.clone()))?;
    let raw = raw.unwrap_or(Object::Null);
    validate_file_specification(x, &raw)
        .map(Some)
        .map_err(|e| e.context(context))
}

/// `validateURLSpecEntry`: the URL specification entry `entry_name`; `Some` of its object unless
/// it is absent or null.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups, with the entry as context.
pub fn validate_url_spec_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    required: bool,
    since: Version,
) -> Result<Option<Object>, Error> {
    let raw = d.find(entry_name.as_bytes()).cloned();
    let context = dict_entry_context(dict_name, entry_name, raw.as_ref());
    let o = validate_entry(x, d, dict_name, entry_name, required, since)
        .map_err(|e| e.context(context.clone()))?;
    if o.is_none() {
        return Ok(None);
    }
    x.validate_version("URLSpec", since)
        .map_err(|e| e.context(context.clone()))?;
    let raw = raw.unwrap_or(Object::Null);
    validate_url_specification(x, &raw)
        .map(Some)
        .map_err(|e| e.context(context))
}

/// `validateFileSpecificationOrFormObject` (13.2.4.2): a file specification, which is reported
/// as `true`, or a form XObject, which is `false`. Relaxed validation accepts a string for the
/// full file specification dictionary.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_file_specification_or_form_object(
    v: &mut Validator<'_>,
    obj: &Object,
) -> Result<bool, Error> {
    let context = object_context("file specification or form object", Some(obj));
    let o =
        v.x.dereference(obj)
            .map_err(|e| e.context(format!("{context}: dereference")))?
            .clone();
    match o {
        Object::Stream(mut sd) => {
            let form = object_context("form XObject", Some(obj));
            super::xobject::validate_xobject_type(v.x, &mut sd)
                .map_err(|e| e.context(format!("{form} Type")))?;
            validate_name_entry(
                v.x,
                &sd.dict,
                "formStreamDict",
                "Subtype",
                REQUIRED,
                Version::V10,
                Some(&|s: &[u8]| s == b"Form"),
            )
            .map_err(|e| e.context(format!("{form} Subtype")))?;
            let owner = match obj {
                Object::Ref(r) => Some(*r),
                _ => None,
            };
            super::xobject::validate_form_stream_dict(v, &sd, owner)?;
            Ok(false)
        }
        Object::Dict(_) | Object::String(_) | Object::Hex(_) => {
            validate_file_specification(v.x, obj)?;
            Ok(true)
        }
        other => Err(Error::Validation(format!(
            "{context}: expected file specification dictionary or Form XObject, got {}",
            type_name(&other)
        ))),
    }
}

/// `isURLFileSpecification` (link_collect.go:215): a dictionary with the file system `URL`.
pub fn is_url_file_specification(x: &mut XRefTable, o: &Object) -> bool {
    let fs = match x.dereference(o) {
        Ok(Object::Dict(d)) => d.find(b"FS").cloned(),
        _ => return false,
    };
    matches!(
        x.name_from_value(b"FS", fs.as_ref()),
        Ok((Some(n), _)) if n == b"URL"
    )
}

/// `isEmbeddedFileSpecification` (link_collect.go:229): a file specification dictionary whose
/// first file name has an embedded file stream in `EF`.
pub fn is_embedded_file_specification(x: &mut XRefTable, o: &Object) -> bool {
    if is_url_file_specification(x, o) {
        return false;
    }
    let d = match x.dereference(o) {
        Ok(Object::Dict(d)) => d.clone(),
        _ => return false,
    };
    let Some(raw_ef) = d.find(b"EF") else {
        return false;
    };
    let ef = match x.dereference_dict(raw_ef) {
        Ok(Some(ef)) => ef.clone(),
        _ => return false,
    };
    let keys: [&[u8]; 5] = [b"UF", b"F", b"DOS", b"Mac", b"Unix"];
    for key in keys {
        if !matches!(x.dereference_string_entry(&d, key), Ok((Some(_), true))) {
            continue;
        }
        let Some(raw_file) = ef.find(key) else {
            continue;
        };
        return matches!(x.dereference(raw_file), Ok(Object::Stream(_)));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::xref::Entry;
    use crate::object::Ref;

    fn table() -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        x
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    #[test]
    fn request_uris() {
        for ok in [
            "http://example.com/a?b=c",
            "https://example.com",
            "https://[::1]:8080/x",
            "http://[fe80::1%25en0]/",
            "http://[::ffff:1.2.3.4]/",
            "ftp://user:pw@host:21/path",
            "mailto:someone@example.com",
            "/just/a/path",
            "*",
            "file:///tmp/x.pdf",
            "x-scheme://a:b:80/",
        ] {
            assert!(request_uri_is_valid(ok), "{ok}");
        }
        for bad in [
            "",
            "relative/path",
            ":foo",
            "http://exa mple.com/",
            "http://host:abc/",
            "http://a:b:c/",
            "x-scheme://a:b:c/",
            "http://[::1/",
            "http://[1.2.3.4]/",
            "http://[::1]x/",
            "http://x/%zz",
            "/a%2",
            "http://u%zz@host/",
            "http://ho%41st/",
            "a\u{1}b:c",
        ] {
            assert!(!request_uri_is_valid(bad), "{bad}");
        }
    }

    #[test]
    fn ipv6_addresses() {
        for ok in [
            "::",
            "::1",
            "1::",
            "1:2:3:4:5:6:7:8",
            "1::8",
            "::1.2.3.4",
            "fe80::1%eth0",
        ] {
            assert!(parse_ipv6(ok.as_bytes()), "{ok}");
        }
        for bad in [
            "1:2:3:4:5:6:7",
            "1:2:3:4:5:6:7:8:9",
            "1::2::3",
            ":1",
            "1:",
            "12345::",
            "::1%",
            "1.2.3.4",
            "::1.2.3",
        ] {
            assert!(!parse_ipv6(bad.as_bytes()), "{bad}");
        }
    }

    #[test]
    fn second_class_names() {
        assert!(is_second_class_name("ABCD_x"));
        assert!(is_second_class_name("ABCD:x"));
        assert!(!is_second_class_name("ABC"));
        assert!(!is_second_class_name("ABCDEFGH"));
        assert!(!is_second_class_name("ABC\u{e9}_"));
    }

    #[test]
    fn simple_file_specifications() {
        let mut x = table();
        assert!(validate_file_specification(&mut x, &Object::string("a.pdf")).is_ok());
        assert!(validate_file_specification(&mut x, &Object::Integer(1)).is_err());
        let d = dict(&[("F", Object::string("a.pdf"))]);
        assert!(validate_file_specification(&mut x, &Object::Dict(d)).is_ok());
        // F is required without UF, DOS, Mac and Unix.
        assert!(validate_file_specification(&mut x, &Object::Dict(Dict::default())).is_err());
        let uf = dict(&[("UF", Object::string("a.pdf"))]);
        assert!(validate_file_specification(&mut x, &Object::Dict(uf)).is_ok());
    }

    #[test]
    fn url_file_specification_checks_the_url() {
        let mut x = table();
        let good = dict(&[
            ("FS", Object::name("URL")),
            ("F", Object::string("https://example.com/a")),
        ]);
        assert!(validate_file_specification(&mut x, &Object::Dict(good.clone())).is_ok());
        assert!(is_url_file_specification(&mut x, &Object::Dict(good)));
        let bad = dict(&[("FS", Object::name("URL")), ("F", Object::string("a b"))]);
        assert!(validate_file_specification(&mut x, &Object::Dict(bad)).is_err());

        assert!(validate_url_specification(&mut x, &Object::Dict(Dict::default())).is_err());
        let spec = dict(&[
            ("FS", Object::name("URL")),
            ("F", Object::string("http://example.com/")),
        ]);
        assert!(validate_url_specification(&mut x, &Object::Dict(spec)).is_ok());
    }

    #[test]
    fn embedded_files() {
        let mut x = table();
        let stream = StreamDict::new(
            dict(&[("Type", Object::name("EmbeddedFile"))]),
            0,
            None,
            None,
            Vec::new(),
        );
        x.table
            .insert(7, Entry::gen0(Object::Stream(Box::new(stream))));
        let ef = dict(&[(
            "F",
            Object::Ref(Ref {
                num: 7,
                generation: 0,
            }),
        )]);
        let spec = dict(&[
            ("Type", Object::name("Filespec")),
            ("F", Object::string("a.txt")),
            ("EF", Object::Dict(ef)),
        ]);
        assert!(validate_file_specification(&mut x, &Object::Dict(spec.clone())).is_ok());
        assert!(is_embedded_file_specification(&mut x, &Object::Dict(spec)));

        let bad_key = dict(&[
            ("F", Object::string("a.txt")),
            ("EF", Object::Dict(dict(&[("X", Object::Null)]))),
        ]);
        assert!(validate_file_specification(&mut x, &Object::Dict(bad_key)).is_err());
    }

    #[test]
    fn file_spec_entry_returns_the_specification() {
        let mut x = table();
        let d = dict(&[("F", Object::string("a.pdf"))]);
        let f = validate_file_spec_entry(&mut x, &d, "GoToR", "F", REQUIRED, Version::V11);
        assert!(matches!(f, Ok(Some(Object::String(_)))));
        let none = validate_file_spec_entry(&mut x, &d, "GoToR", "X", OPTIONAL, Version::V11);
        assert!(matches!(none, Ok(None)));
        let missing = validate_file_spec_entry(&mut x, &d, "GoToR", "X", REQUIRED, Version::V11);
        assert!(missing.is_err());
    }
}
