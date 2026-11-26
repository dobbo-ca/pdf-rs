//! Escaping, name encoding and hex helpers on byte strings (Go: types/string.go). PDF strings
//! and names are bytes, so every function takes and returns `[u8]`.

use crate::error::Error;
use std::borrow::Cow;

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// `ByteForOctalString`: a one- to three-digit octal string as a byte. The high-order overflow is
/// dropped, as PDF literal strings require; invalid input is 0.
pub fn byte_for_octal_string(octal: &[u8]) -> u8 {
    if octal.is_empty() || octal.len() > 3 {
        return 0;
    }
    let mut v: u32 = 0;
    for &c in octal {
        if !(b'0'..=b'7').contains(&c) {
            return 0;
        }
        v = v * 8 + u32::from(c - b'0');
    }
    (v & 0xff) as u8
}

/// `Escape`: applies the escape sequences of a literal string.
pub fn escape(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for &c in s {
        let e = match c {
            0x0A => b'n',
            0x0D => b'r',
            0x09 => b't',
            0x08 => b'b',
            0x0C => b'f',
            b'\\' | b'(' | b')' => c,
            _ => {
                out.push(c);
                continue;
            }
        };
        out.push(b'\\');
        out.push(e);
    }
    out
}

/// `escaped`: the byte an escape letter stands for, and whether it starts an octal code.
fn escaped(c: u8) -> (bool, u8) {
    match c {
        b'n' => (false, 0x0A),
        b'r' => (false, 0x0D),
        b't' => (false, 0x09),
        b'b' => (false, 0x08),
        b'f' => (false, 0x0C),
        b'0'..=b'7' => (true, c),
        _ => (false, c),
    }
}

/// `Unescape`: resolves every escape sequence of the body of a literal string. Go's version also
/// returned an error for a `\` inside an octal code; that cannot happen (the code is closed
/// before the `\` is looked at), so this one cannot fail.
pub fn unescape(s: &[u8]) -> Vec<u8> {
    let mut esc = false;
    let mut long_eol = false;
    let mut octal: Vec<u8> = Vec::new();
    let mut out = Vec::new();

    for &c in s {
        if long_eol {
            esc = false;
            long_eol = false;
            // c is completing a 0x5C0D0A line break.
            if c == 0x0A {
                continue;
            }
        }

        if !octal.is_empty() {
            if (b'0'..=b'7').contains(&c) {
                octal.push(c);
                if octal.len() == 3 {
                    out.push(byte_for_octal_string(&octal));
                    octal.clear();
                    esc = false;
                }
                continue;
            }
            out.push(byte_for_octal_string(&octal));
            octal.clear();
            esc = false;
        }

        // regularChar
        if c != 0x5c && !esc {
            out.push(c);
            continue;
        }

        if c == 0x5c {
            if esc {
                // Escaped \
                out.push(c);
                esc = false;
            } else {
                esc = true;
            }
            continue;
        }

        // esc is true and c is not \.

        // Ignore \eol line breaks.
        if c == 0x0A {
            esc = false;
            continue;
        }

        if c == 0x0D {
            long_eol = true;
            continue;
        }

        let (is_octal, c) = escaped(c);
        if is_octal {
            octal.push(c);
            continue;
        }

        out.push(c);
        esc = false;
    }

    if !octal.is_empty() {
        out.push(byte_for_octal_string(&octal));
    }
    out
}

/// "7.3.5 Name Objects": whether a byte must be written as `#xx`.
fn needs_hex_sequence(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%' | b'#'
    ) || !(b'!'..=b'~').contains(&c)
}

/// The two lower-case hex digits of `c`.
fn push_hex(out: &mut Vec<u8>, c: u8) {
    out.push(HEX_DIGITS[usize::from(c >> 4)]);
    out.push(HEX_DIGITS[usize::from(c & 15)]);
}

/// `EncodeName`: name encoding according to the PDF spec. Borrows `s` when nothing changes.
pub fn encode_name(s: &[u8]) -> Cow<'_, [u8]> {
    if !s.iter().any(|&c| needs_hex_sequence(c)) {
        return Cow::Borrowed(s);
    }
    let mut out = Vec::new();
    for &c in s {
        if needs_hex_sequence(c) {
            out.push(b'#');
            push_hex(&mut out, c);
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `DecodeName`: name decoding according to the PDF spec. Borrows `s` when nothing changes.
///
/// # Errors
/// A NUL byte (also as `#00`), fewer than two characters after `#`, or a non-hex character
/// ([`Error::Hex`]).
pub fn decode_name(s: &[u8]) -> Result<Cow<'_, [u8]>, Error> {
    let mut out: Option<Vec<u8>> = None;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == 0 {
            return Err(Error::Corrupt("a name may not contain a null byte".into()));
        }
        if c != b'#' {
            if let Some(o) = out.as_mut() {
                o.push(c);
            }
            i += 1;
            continue;
        }

        // # detected, the next 2 chars have to exist, and be hex characters.
        let (Some(&p), Some(&q)) = (s.get(i + 1), s.get(i + 2)) else {
            return Err(Error::Corrupt("not enough characters after #".into()));
        };
        let Some(a) = hex_val(p) else {
            return Err(Error::Hex(p));
        };
        let Some(b) = hex_val(q) else {
            return Err(Error::Hex(q));
        };
        let decoded = (a << 4) | b;
        if decoded == 0 {
            return Err(Error::Corrupt("a name may not contain a null byte".into()));
        }
        out.get_or_insert_with(|| s[..i].to_vec()).push(decoded);
        i += 3;
    }
    Ok(match out {
        Some(v) => Cow::Owned(v),
        None => Cow::Borrowed(s),
    })
}

/// `TrimLeadingComment`: empty when the first non-blank byte starts a comment (or there is none),
/// else `s` itself (not `s` without its blanks).
pub fn trim_leading_comment(s: &[u8]) -> &[u8] {
    for &c in s {
        match c {
            b' ' | b'\t' | b'\r' | b'\n' | 0x0C => continue,
            b'%' => return &[],
            _ => return s,
        }
    }
    &[]
}

/// `hex.EncodeToString` as bytes: lower-case digits.
pub fn hex_encode(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for &c in b {
        push_hex(&mut out, c);
    }
    out
}

/// `hex.DecodeString` (digits of either case).
///
/// # Errors
/// [`Error::Hex`] for the first invalid digit; an odd number of valid digits is
/// [`Error::Corrupt`] ("encoding/hex: odd length hex string").
pub fn hex_decode(s: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let (pairs, rest) = s.as_chunks::<2>();
    for &[p, q] in pairs {
        let Some(a) = hex_val(p) else {
            return Err(Error::Hex(p));
        };
        let Some(b) = hex_val(q) else {
            return Err(Error::Hex(q));
        };
        out.push((a << 4) | b);
    }
    if let Some(&last) = rest.first() {
        if hex_val(last).is_none() {
            return Err(Error::Hex(last));
        }
        return Err(Error::Corrupt("encoding/hex: odd length hex string".into()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_for_octal_string_table() {
        let cases: [(&[u8], u8); 11] = [
            (b"001", 0x1),
            (b"01", 0x1),
            (b"1", 0x1),
            (b"010", 0x8),
            (b"020", 0x10),
            (b"377", 0xff),
            (b"400", 0x00),
            (b"777", 0xff),
            (b"", 0x00),
            (b"8", 0x00),
            (b"1000", 0x00),
        ];
        for (input, want) in cases {
            assert_eq!(byte_for_octal_string(input), want, "{input:?}");
        }
    }

    #[test]
    fn unescape_with_octal() {
        let cases: [(&[u8], &[u8]); 9] = [
            (b"\\5", &[0x05]),
            (b"\\5a", &[0x05, b'a']),
            (b"\\5\\5", &[0x05, 0x05]),
            (b"\\53", b"+"),
            (b"\\53a", b"+a"),
            (b"\\053", b"+"),
            (b"\\0053", &[0x05, b'3']),
            (b"\\400", &[0x00]),
            (b"\\777", &[0xff]),
        ];
        for (input, want) in cases {
            assert_eq!(unescape(input), want, "{input:?}");
        }
    }

    #[test]
    fn unescape_line_breaks_and_letters() {
        assert_eq!(unescape(b"a\\\nb"), b"ab");
        assert_eq!(unescape(b"a\\\r\nb"), b"ab");
        assert_eq!(unescape(b"a\\\rb"), b"ab");
        assert_eq!(
            unescape(b"\\n\\r\\t\\b\\f\\(\\)\\\\"),
            b"\n\r\t\x08\x0c()\\"
        );
        // "\ " and "\a" keep the character.
        assert_eq!(unescape(b"\\ \\a"), b" a");
    }

    #[test]
    fn escape_round_trip() {
        let raw = b"a(b)c\\d\n\r\t\x08\x0c\x01";
        let e = escape(raw);
        assert_eq!(e, b"a\\(b\\)c\\\\d\\n\\r\\t\\b\\f\x01");
        assert_eq!(unescape(&e), raw);
    }

    #[test]
    fn decode_name_table() {
        let cases = [
            ("", ""),
            ("Size", "Size"),
            ("S#69#7a#65", "Size"),
            ("#52#6f#6f#74", "Root"),
            ("#4f#75t#6c#69#6e#65#73", "Outlines"),
            ("C#6fu#6et", "Count"),
            ("K#69#64s", "Kids"),
            ("#50a#72e#6et", "Parent"),
            ("#4d#65di#61#42#6f#78", "MediaBox"),
            ("#46#69#6c#74er", "Filter"),
            ("#46#6ca#74e#44#65c#6fde", "FlateDecode"),
            ("A#53#43#49I#48e#78D#65code", "ASCIIHexDecode"),
        ];
        for (input, want) in cases {
            let got = decode_name(input.as_bytes());
            assert_eq!(got.as_deref().ok(), Some(want.as_bytes()), "{input}");
        }
    }

    #[test]
    fn decode_name_errors() {
        assert!(decode_name(b"A\0").is_err());
        assert!(decode_name(b"A#00").is_err());
        assert!(decode_name(b"A#4").is_err());
        assert!(decode_name(b"A#").is_err());
        assert_eq!(decode_name(b"A#4g").err(), Some(Error::Hex(b'g')));
        assert_eq!(decode_name(b"A#g4").err(), Some(Error::Hex(b'g')));
    }

    #[test]
    fn encode_name_table() {
        let cases = [
            ("Foo", "Foo"),
            ("A#", "A#23"),
            ("F#o", "F#23o"),
            (
                "A;Name_With-Various***Characters?",
                "A;Name_With-Various***Characters?",
            ),
            ("1.2", "1.2"),
            ("$$", "$$"),
            ("@pattern", "@pattern"),
            (".notdef", ".notdef"),
            ("Lime Green", "Lime#20Green"),
            ("paired()parentheses", "paired#28#29parentheses"),
            ("The_Key_of_F#_Minor", "The_Key_of_F#23_Minor"),
        ];
        for (input, want) in cases {
            assert_eq!(&*encode_name(input.as_bytes()), want.as_bytes(), "{input}");
        }
        assert!(matches!(encode_name(b"Foo"), Cow::Borrowed(_)));
        assert_eq!(&*encode_name(&[0x00, 0x7f, 0xff]), b"#00#7f#ff");
    }

    #[test]
    fn trim_leading_comment_quirk() {
        assert_eq!(trim_leading_comment(b"  % c"), b"");
        assert_eq!(trim_leading_comment(b"   "), b"");
        // The whole input comes back, blanks included.
        assert_eq!(trim_leading_comment(b"  x % c"), b"  x % c");
    }

    #[test]
    fn hex_round_trip_and_errors() {
        assert_eq!(hex_encode(&[0x00, 0xAB, 0xff]), b"00abff");
        assert_eq!(hex_decode(b"00aBFf").ok(), Some(vec![0x00, 0xAB, 0xff]));
        assert_eq!(hex_decode(b"0g").err(), Some(Error::Hex(b'g')));
        assert_eq!(hex_decode(b"0ag").err(), Some(Error::Hex(b'g')));
        assert!(matches!(hex_decode(b"0a0"), Err(Error::Corrupt(_))));
        assert_eq!(hex_decode(b"").ok(), Some(Vec::new()));
    }
}
