//! Text strings: UTF-16BE, PDFDocEncoding and the conversions between a PDF string object and
//! Rust text (Go: types/utf16.go, types/pdfDocEncoding.go).
//!
//! The error messages of Go that print the offending bytes are shortened to say what is wrong
//! (messages never carry document content, see `Error`).

use super::Object;
use super::string::{escape, hex_decode, unescape};
use crate::error::Error;

fn invalid_utf16be() -> Error {
    Error::Corrupt("invalid UTF-16BE detected".into())
}

fn invalid_utf8() -> Error {
    Error::Corrupt("invalid UTF-8".into())
}

/// `IsStringUTF16BE`: starts with the big-endian byte order mark.
pub fn is_string_utf16be(s: &[u8]) -> bool {
    s.starts_with(&[0xFE, 0xFF])
}

/// `IsUTF16BE`: a big-endian byte order mark and an even length.
pub fn is_utf16be(b: &[u8]) -> bool {
    !b.is_empty() && b.len().is_multiple_of(2) && b.starts_with(&[0xFE, 0xFF])
}

/// `decodeUTF16String` / `DecodeUTF16String`: UTF-16BE with a byte order mark to text. Only valid
/// surrogate pairs are accepted.
///
/// # Errors
/// [`Error::Corrupt`] for a missing byte order mark, an odd length or a bad surrogate.
pub fn decode_utf16_string(b: &[u8]) -> Result<String, Error> {
    if !is_utf16be(b) {
        return Err(invalid_utf16be());
    }
    // Strip the BOM.
    let mut rest = &b[2..];
    let mut units: Vec<u16> = Vec::new();

    while let [hi, lo, tail @ ..] = rest {
        let val = u16::from_be_bytes([*hi, *lo]);
        if !(0xD800..=0xDFFF).contains(&val) {
            // Basic Multilingual Plane
            units.push(val);
            rest = tail;
            continue;
        }

        // Ensure the bytes needed in order to decode a surrogate pair.
        if tail.is_empty() {
            return Err(Error::Corrupt(
                "corrupt UTF16BE byte length on unicode point 1".into(),
            ));
        }

        // Ensure the high surrogate leads in the possible surrogate pair.
        if val >= 0xDC00 {
            return Err(Error::Corrupt("corrupt UTF16BE on unicode point 1".into()));
        }

        // Supplementary Planes
        units.push(val);
        let [hi2, lo2, tail2 @ ..] = tail else {
            return Err(Error::Corrupt(
                "corrupt UTF16BE byte length on unicode point 2".into(),
            ));
        };
        let val2 = u16::from_be_bytes([*hi2, *lo2]);
        if !(0xDC00..=0xDFFF).contains(&val2) {
            return Err(Error::Corrupt("corrupt UTF16BE on unicode point 2".into()));
        }
        units.push(val2);
        rest = tail2;
    }

    Ok(char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect())
}

/// `EncodeUTF16String`: text as UTF-16BE with a byte order mark.
pub fn encode_utf16_string(s: &str) -> Vec<u8> {
    let mut out = vec![0xFE, 0xFF];
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_be_bytes());
    }
    out
}

/// `EscapedUTF16String`: the escaped UTF-16BE string for the UTF-8 text `s`.
///
/// # Errors
/// [`Error::Corrupt`] ("invalid UTF-8") if `s` is not valid UTF-8.
pub fn escaped_utf16_string(s: &[u8]) -> Result<Vec<u8>, Error> {
    let s = std::str::from_utf8(s).map_err(|_| invalid_utf8())?;
    Ok(escape(&encode_utf16_string(s)))
}

/// Table D.2 of ISO 32000-1:2008 for the bytes that differ from Latin-1 below 0xA1.
fn pdf_doc_special(b: u8) -> Option<char> {
    let u: u32 = match b {
        0x18 => 0x02d8,
        0x19 => 0x02c7,
        0x1a => 0x02c6,
        0x1b => 0x02d9,
        0x1c => 0x02dd,
        0x1d => 0x02db,
        0x1e => 0x02da,
        0x1f => 0x02dc,
        0x80 => 0x2022,
        0x81 => 0x2020,
        0x82 => 0x2021,
        0x83 => 0x2026,
        0x84 => 0x2014,
        0x85 => 0x2013,
        0x86 => 0x0192,
        0x87 => 0x2044,
        0x88 => 0x2039,
        0x89 => 0x203a,
        0x8a => 0x2212,
        0x8b => 0x2030,
        0x8c => 0x201e,
        0x8d => 0x201c,
        0x8e => 0x201d,
        0x8f => 0x2018,
        0x90 => 0x2019,
        0x91 => 0x201a,
        0x92 => 0x2122,
        0x93 => 0xfb01,
        0x94 => 0xfb02,
        0x95 => 0x0141,
        0x96 => 0x0152,
        0x97 => 0x0160,
        0x98 => 0x0178,
        0x99 => 0x017d,
        0x9a => 0x0131,
        0x9b => 0x0142,
        0x9c => 0x0153,
        0x9d => 0x0161,
        0x9e => 0x017e,
        0xa0 => 0x20ac,
        _ => return None,
    };
    char::from_u32(u)
}

/// `pdfDocEncodingRune`: U+FFFD for the bytes the encoding leaves undefined.
pub fn pdf_doc_encoding_char(b: u8) -> char {
    if let Some(c) = pdf_doc_special(b) {
        return c;
    }
    if matches!(b, 0x09 | 0x0a | 0x0d | 0x20..=0x7e | 0xa1..=0xff) && b != 0xad {
        return char::from(b);
    }
    char::REPLACEMENT_CHARACTER
}

/// `decodePDFDocEncoding`.
pub fn decode_pdf_doc_encoding(b: &[u8]) -> String {
    b.iter().map(|&c| pdf_doc_encoding_char(c)).collect()
}

/// `isPDFDocEncodingASCII`: whether `b` can keep its ASCII byte representation.
fn is_pdf_doc_encoding_ascii(b: &[u8]) -> bool {
    b.iter().all(|&c| c < 0x80 && !(0x18..=0x1f).contains(&c))
}

/// `decodeTextString`: PDFDocEncoding or BOM-marked Unicode bytes (7.9.2.2).
fn decode_text_string(bb: &[u8]) -> Result<String, Error> {
    if bb.starts_with(&[0xFE, 0xFF]) {
        return decode_utf16_string(bb);
    }
    if let Some(rest) = bb.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return std::str::from_utf8(rest)
            .map(str::to_owned)
            .map_err(|_| invalid_utf8());
    }
    // Preserve the ASCII control bytes callers use for name-tree keys.
    if is_pdf_doc_encoding_ascii(bb) {
        return Ok(bb.iter().map(|&c| char::from(c)).collect());
    }
    Ok(decode_pdf_doc_encoding(bb))
}

/// `StringLiteralToString`: unescapes the body of a literal string and decodes its text
/// encoding.
///
/// # Errors
/// As [`decode_utf16_string`], or "invalid UTF-8" after a UTF-8 byte order mark.
pub fn string_literal_to_string(sl: &[u8]) -> Result<String, Error> {
    decode_text_string(&unescape(sl))
}

/// `HexLiteralToString`: decodes the digits of a hex string and its text encoding.
///
/// # Errors
/// An invalid hex digit, or as [`string_literal_to_string`].
pub fn hex_literal_to_string(hl: &[u8]) -> Result<String, Error> {
    decode_text_string(&hex_decode(hl)?)
}

/// `StringOrHexLiteral`: the text of a string or hex string object.
///
/// # Errors
/// [`Error::Corrupt`] ("expected StringLiteral or HexLiteral") for any other object.
pub fn string_or_hex_literal(obj: &Object) -> Result<String, Error> {
    match obj {
        Object::String(s) => string_literal_to_string(s),
        Object::Hex(h) => hex_literal_to_string(h),
        _ => Err(Error::Corrupt(
            "expected StringLiteral or HexLiteral".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_surrogate_boundary() {
        let cases: [(&str, &[u8], &str, bool); 7] = [
            ("ASCII", &[0xFE, 0xFF, 0x00, 0x41], "A", false),
            ("LastHighSurrogate", &[0xFE, 0xFF, 0xDB, 0xFF], "", true),
            ("FirstLowSurrogate", &[0xFE, 0xFF, 0xDC, 0x00], "", true),
            ("LastLowSurrogate", &[0xFE, 0xFF, 0xDF, 0xFF], "", true),
            (
                "FirstPrivateUse",
                &[0xFE, 0xFF, 0xE0, 0x00],
                "\u{E000}",
                false,
            ),
            (
                "SecondPrivateUse",
                &[0xFE, 0xFF, 0xE0, 0x01],
                "\u{E001}",
                false,
            ),
            (
                "ValidSurrogatePair",
                &[0xFE, 0xFF, 0xD8, 0x3D, 0xDE, 0x00],
                "\u{1F600}",
                false,
            ),
        ];
        for (name, encoded, want, want_err) in cases {
            let got = decode_utf16_string(encoded);
            assert_eq!(got.is_err(), want_err, "{name}");
            if !want_err {
                assert_eq!(got.ok().as_deref(), Some(want), "{name}");
            }
        }
    }

    #[test]
    fn utf16_framing_errors() {
        // No BOM, odd length, empty.
        assert!(decode_utf16_string(&[0x00, 0x41]).is_err());
        assert!(decode_utf16_string(&[0xFE, 0xFF, 0x00]).is_err());
        assert!(decode_utf16_string(&[]).is_err());
        // A BOM alone is the empty string.
        assert_eq!(decode_utf16_string(&[0xFE, 0xFF]).ok().as_deref(), Some(""));
        // A high surrogate followed by a non-low unit.
        assert!(decode_utf16_string(&[0xFE, 0xFF, 0xD8, 0x3D, 0x00, 0x41]).is_err());
    }

    #[test]
    fn utf16_round_trip() {
        let s = "625,50 \u{20ac} \u{1F600}";
        let enc = encode_utf16_string(s);
        assert_eq!(&enc[..2], &[0xFEu8, 0xFF]);
        assert_eq!(decode_utf16_string(&enc).ok().as_deref(), Some(s));
    }

    #[test]
    fn escaped_utf16_string_rejects_invalid_utf8() {
        assert_eq!(
            escaped_utf16_string(&[0xFF]).err(),
            Some(Error::Corrupt("invalid UTF-8".into()))
        );
        // "(" is 0x00 0x28 after the BOM; the parenthesis is escaped.
        assert_eq!(
            escaped_utf16_string(b"(").ok(),
            Some(vec![0xFE, 0xFF, 0x00, b'\\', b'(']),
        );
    }

    #[test]
    fn string_literal_to_string_pdf_doc_encoding() {
        let enc = encode_utf16_string("625,50 \u{20ac}");
        let cases: [(&str, Vec<u8>, &str); 4] = [
            ("ASCII", b"625,50".to_vec(), "625,50"),
            ("Euro", b"625,50 \xa0".to_vec(), "625,50 \u{20ac}"),
            ("UTF8", b"\xef\xbb\xbfcaf\xc3\xa9".to_vec(), "caf\u{e9}"),
            ("UTF16BE", enc, "625,50 \u{20ac}"),
        ];
        for (name, input, want) in cases {
            let got = string_literal_to_string(&input);
            assert_eq!(got.ok().as_deref(), Some(want), "{name}");
        }
    }

    #[test]
    fn hex_literal_to_string_pdf_doc_encoding() {
        let got = hex_literal_to_string(b"3632352c353020a0");
        assert_eq!(got.ok().as_deref(), Some("625,50 \u{20ac}"));
        assert!(hex_literal_to_string(b"zz").is_err());
    }

    #[test]
    fn pdf_doc_encoding_gaps() {
        assert_eq!(pdf_doc_encoding_char(0x00), '\u{FFFD}');
        assert_eq!(pdf_doc_encoding_char(0x9f), '\u{FFFD}');
        assert_eq!(pdf_doc_encoding_char(0xad), '\u{FFFD}');
        assert_eq!(pdf_doc_encoding_char(0x18), '\u{02d8}');
        assert_eq!(pdf_doc_encoding_char(0xff), '\u{ff}');
        assert_eq!(pdf_doc_encoding_char(0x0a), '\n');
        // A control byte in the 0x18..=0x1f range is not ASCII-preserved.
        assert_eq!(
            string_literal_to_string(&[0x41, 0x1c]).ok().as_deref(),
            Some("A\u{02dd}")
        );
        // Other control bytes are.
        assert_eq!(
            string_literal_to_string(&[0x41, 0x01]).ok().as_deref(),
            Some("A\u{1}")
        );
    }

    #[test]
    fn string_or_hex_literal_dispatch() {
        let s = Object::String(b"a\\(b".to_vec());
        assert_eq!(string_or_hex_literal(&s).ok().as_deref(), Some("a(b"));
        let h = Object::Hex(b"4142".to_vec());
        assert_eq!(string_or_hex_literal(&h).ok().as_deref(), Some("AB"));
        assert!(string_or_hex_literal(&Object::Integer(1)).is_err());
    }
}
