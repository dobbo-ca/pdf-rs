//! Explicit metrics for the standard fonts, which PDF 2.0 requires (Go:
//! pkg/pdfcpu/font/fontDict.go `EnsureCoreFontMetrics` and its helpers, 9.6.2.1 Table 109). The
//! tables are in `corefont_data`.

use super::corefont_data::{
    CORE_FONTS, FontMetrics, STANDARD_GLYPHS, SYMBOL_GLYPHS, WIN_ANSI_GLYPHS, ZAPF_DINGBATS_GLYPHS,
};
use crate::error::Error;
use crate::model::XRefTable;
use crate::model::deref::NULL;
use crate::object::{Array, Dict, Object, Ref};

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Go's `d[key]` on a dictionary: the object stored under exactly `key`, nil if there is none.
fn index<'a>(d: &'a Dict, key: &[u8]) -> &'a Object {
    d.0.get(key).unwrap_or(&NULL)
}

/// `metrics.CoreFontMetrics[name]`.
fn metrics_of(name: &[u8]) -> Option<&'static FontMetrics> {
    CORE_FONTS
        .iter()
        .find(|(n, _)| n.as_bytes() == name)
        .map(|(_, m)| m)
}

/// `coreFontDescriptor`: a new font descriptor object for the standard font `name`.
fn core_font_descriptor(x: &mut XRefTable, name: &[u8], fm: &FontMetrics) -> Result<Ref, Error> {
    // Nonsymbolic.
    let mut flags: i64 = 32;
    let (mut ascent, mut descent) = (fm.ascent, fm.descent);
    if name == b"Symbol" || name == b"ZapfDingbats" {
        // Symbolic; these AFMs omit ascender and descender metrics.
        flags = 4;
        ascent = fm.fbox[3];
        descent = fm.fbox[1];
    }
    if name.starts_with(b"Courier") {
        // FixedPitch and Serif.
        flags |= 3;
    }
    if name.starts_with(b"Times") {
        // Serif.
        flags |= 2;
    }
    if fm.italic_angle != 0.0 {
        // Italic.
        flags |= 64;
    }
    let mut d = Dict::default();
    d.set(b"Type", Object::name("FontDescriptor"));
    d.set(b"FontName", Object::Name(name.to_vec()));
    d.set(b"Flags", Object::Integer(flags));
    d.set(
        b"FontBBox",
        Object::Array(crate::object::array::new_number_array(&fm.fbox)),
    );
    d.set(b"ItalicAngle", Object::Float(fm.italic_angle));
    d.set(b"Ascent", Object::Float(ascent));
    d.set(b"Descent", Object::Float(descent));
    d.set(b"StemV", Object::Float(fm.stem_v));
    if fm.cap_height != 0.0 {
        d.set(b"CapHeight", Object::Float(fm.cap_height));
    }
    x.ind_ref_for_new_object(Object::Dict(d)).map_err(|e| {
        e.context(format!(
            "font {}: create core font descriptor: insert object",
            lossy(name)
        ))
    })
}

/// `coreFontGlyphMap`: the glyph names of the codes of `encoding` (empty for the built-in
/// encoding of the font).
fn core_font_glyph_map(
    font_name: &[u8],
    encoding: &[u8],
) -> Result<&'static [(u8, &'static str)], Error> {
    let m: &'static [(u8, &'static str)] = if encoding == b"WinAnsiEncoding" {
        &WIN_ANSI_GLYPHS
    } else if !encoding.is_empty() {
        return Err(Error::Corrupt(format!(
            "font {}: cannot reconstruct metrics for encoding {}",
            lossy(font_name),
            lossy(encoding)
        )));
    } else if font_name == b"Symbol" {
        &SYMBOL_GLYPHS
    } else if font_name == b"ZapfDingbats" {
        &ZAPF_DINGBATS_GLYPHS
    } else {
        &STANDARD_GLYPHS
    };
    Ok(m)
}

/// `coreFontEncoding`: the name of the encoding of the font dictionary `d` (the base encoding of
/// an encoding dictionary), and the `Differences` of the latter.
fn core_font_encoding(x: &mut XRefTable, d: &Dict) -> Result<(Vec<u8>, Array), Error> {
    let o = x.dereference(index(d, b"Encoding"))?.clone();
    match o {
        Object::Null => Ok((Vec::new(), Array::new())),
        Object::Name(n) => Ok((n, Array::new())),
        Object::Dict(o) => {
            let (base, _) = x.dereference_name_entry(&o, b"BaseEncoding")?;
            let encoding = base.unwrap_or_default();
            let diff = x
                .dereference_array(index(&o, b"Differences"))?
                .cloned()
                .unwrap_or_default();
            Ok((encoding, diff))
        }
        _ => Err(Error::Corrupt("invalid core font encoding".into())),
    }
}

/// `applyCoreFontDifferences`.
fn apply_core_font_differences(
    x: &mut XRefTable,
    glyphs: &mut [Vec<u8>],
    diff: Array,
) -> Result<(), Error> {
    let mut c: i64 = -1;
    for o in diff {
        let o = x.dereference(&o)?.clone();
        match o {
            Object::Integer(i) => {
                c = i;
                if !(0..=255).contains(&c) {
                    return Err(Error::Corrupt(
                        "core font Differences code outside 0..255".into(),
                    ));
                }
            }
            Object::Name(n) => {
                let slot = match usize::try_from(c) {
                    Ok(i) => glyphs.get_mut(i),
                    Err(_) => None,
                };
                let Some(slot) = slot else {
                    return Err(Error::Corrupt(
                        "core font Differences name without valid code".into(),
                    ));
                };
                *slot = n;
                c += 1;
            }
            _ => {
                return Err(Error::Corrupt("invalid core font Differences entry".into()));
            }
        }
    }
    Ok(())
}

/// `coreFontWidths`: the widths of the codes 0 to 255 of the standard font `font_name`.
fn core_font_widths(
    x: &mut XRefTable,
    d: &Dict,
    font_name: &[u8],
    fm: &FontMetrics,
) -> Result<Array, Error> {
    let (encoding, diff) = core_font_encoding(x, d)?;
    let m = core_font_glyph_map(font_name, &encoding)?;
    let mut glyphs: Vec<Vec<u8>> = vec![Vec::new(); 256];
    for (c, name) in m {
        if let Some(slot) = glyphs.get_mut(usize::from(*c)) {
            *slot = name.as_bytes().to_vec();
        }
    }
    apply_core_font_differences(x, &mut glyphs, diff)?;
    let mut widths = Array::new();
    widths.try_reserve_exact(glyphs.len())?;
    for name in &glyphs {
        let w = fm.width(name);
        if w.is_none() && !name.is_empty() && name != b".notdef" {
            return Err(Error::Corrupt(format!(
                "font {}: no metrics for glyph {}",
                lossy(font_name),
                lossy(name)
            )));
        }
        widths.push(Object::Integer(w.unwrap_or(0)));
    }
    Ok(widths)
}

/// `missingCoreFontMetrics`: how many of `FirstChar`, `LastChar`, `Widths` and `FontDescriptor`
/// the font dictionary lacks.
fn missing_core_font_metrics(x: &mut XRefTable, d: &Dict) -> Result<usize, Error> {
    let mut missing = 0;
    for key in [
        b"FirstChar".as_slice(),
        b"LastChar",
        b"Widths",
        b"FontDescriptor",
    ] {
        if x.dereference(index(d, key))?.is_null() {
            missing += 1;
        }
    }
    Ok(missing)
}

/// `EnsureCoreFontMetrics`: completes the omitted metrics of a standard Type 1 font dictionary,
/// which PDF 2.0 needs. Complete metrics are kept; partial metrics and unsupported encodings are
/// errors. Returns the new font descriptor if the dictionary was changed.
///
/// # Errors
/// [`Error::Corrupt`] for partial metrics, an encoding that is neither WinAnsi nor built in, an
/// invalid `Differences`, or a glyph without a width; the errors of the lookups.
pub fn ensure_core_font_metrics(x: &mut XRefTable, d: &mut Dict) -> Result<Option<Ref>, Error> {
    let (subtype, _) = x.dereference_name_entry(d, b"Subtype")?;
    if subtype.as_deref() != Some(b"Type1".as_slice()) {
        return Ok(None);
    }
    let (name, _) = x.dereference_name_entry(d, b"BaseFont")?;
    let Some(name) = name else {
        return Ok(None);
    };
    let Some(fm) = metrics_of(&name) else {
        return Ok(None);
    };
    let missing = missing_core_font_metrics(x, d)?;
    if missing == 0 {
        return Ok(None);
    }
    if missing != 4 {
        return Err(Error::Corrupt(format!(
            "font {}: cannot reconstruct partial standard-font metrics",
            lossy(&name)
        )));
    }
    let widths = core_font_widths(x, d, &name, fm)?;
    let fd = core_font_descriptor(x, &name, fm)?;
    d.insert_int(b"FirstChar", 0);
    d.insert_int(b"LastChar", 255);
    d.set(b"Widths", Object::Array(widths));
    d.set(b"FontDescriptor", Object::Ref(fd));
    Ok(Some(fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::{Entry, Version};

    fn table() -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.table.insert(0, Entry::free_head());
        x.size = Some(1);
        x.header_version = Some(Version::V20);
        x
    }

    fn font(base: &str, extra: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        d.set(b"Type", Object::name("Font"));
        d.set(b"Subtype", Object::name("Type1"));
        d.set(b"BaseFont", Object::name(base));
        for (k, v) in extra {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn ints(d: &Dict) -> Vec<i64> {
        let Some(Object::Array(a)) = d.find(b"Widths") else {
            panic!("no widths");
        };
        a.iter()
            .map(|o| match o {
                Object::Integer(i) => *i,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn the_tables_are_complete() {
        assert_eq!(CORE_FONTS.len(), 14);
        for (name, fm) in &CORE_FONTS {
            assert!(!fm.w.is_empty(), "{name}");
            assert!(fm.w.windows(2).all(|p| p[0].0 < p[1].0), "{name} sorted");
        }
        assert_eq!(WIN_ANSI_GLYPHS.len(), 216);
        assert_eq!(SYMBOL_GLYPHS.len(), 189);
        assert_eq!(ZAPF_DINGBATS_GLYPHS.len(), 188);
        assert_eq!(STANDARD_GLYPHS.len(), 149);
        let h = metrics_of(b"Helvetica").unwrap();
        assert_eq!(h.width(b"space"), Some(278));
        assert_eq!(h.width(b"nosuchglyph"), None);
        assert!(metrics_of(b"Arial").is_none());
    }

    #[test]
    fn courier_gets_metrics_and_a_descriptor() {
        let mut x = table();
        let mut d = font("Courier", &[("Encoding", Object::name("WinAnsiEncoding"))]);
        let fd = ensure_core_font_metrics(&mut x, &mut d).unwrap().unwrap();
        assert_eq!(d.int_entry(b"FirstChar"), Some(0));
        assert_eq!(d.int_entry(b"LastChar"), Some(255));
        assert_eq!(d.indirect_ref_entry(b"FontDescriptor"), Some(fd));
        let w = ints(&d);
        assert_eq!(w.len(), 256);
        // 'A' is 65; every Courier glyph is 600 wide; unmapped codes are 0.
        assert_eq!(w[65], 600);
        assert_eq!(w[0], 0);

        let desc = x.dereference_dict_ref(fd).unwrap().unwrap().clone();
        assert_eq!(desc.name_entry(b"FontName"), Some(b"Courier".as_slice()));
        // Nonsymbolic, FixedPitch and Serif.
        assert_eq!(desc.int_entry(b"Flags"), Some(32 | 3));
        assert!(desc.has_entry(b"CapHeight"));
    }

    #[test]
    fn symbol_flags_and_bounding_box_metrics() {
        let mut x = table();
        let mut d = font("Symbol", &[]);
        let fd = ensure_core_font_metrics(&mut x, &mut d).unwrap().unwrap();
        let desc = x.dereference_dict_ref(fd).unwrap().unwrap().clone();
        assert_eq!(desc.int_entry(b"Flags"), Some(4));
        let fm = metrics_of(b"Symbol").unwrap();
        assert!(matches!(desc.find(b"Ascent"), Some(Object::Float(a)) if *a == fm.fbox[3]));
        assert!(matches!(desc.find(b"Descent"), Some(Object::Float(a)) if *a == fm.fbox[1]));
        // Symbol has no cap height.
        assert!(!desc.has_entry(b"CapHeight"));
    }

    #[test]
    fn italic_times_flags() {
        let mut x = table();
        let mut d = font("Times-Italic", &[]);
        let fd = ensure_core_font_metrics(&mut x, &mut d).unwrap().unwrap();
        let desc = x.dereference_dict_ref(fd).unwrap().unwrap().clone();
        assert_eq!(desc.int_entry(b"Flags"), Some(32 | 2 | 64));
    }

    #[test]
    fn complete_metrics_are_kept() {
        let mut x = table();
        let mut d = font(
            "Helvetica",
            &[
                ("FirstChar", Object::Integer(32)),
                ("LastChar", Object::Integer(33)),
                (
                    "Widths",
                    Object::Array(vec![Object::Integer(1), Object::Integer(2)]),
                ),
                ("FontDescriptor", Object::Dict(Dict::default())),
            ],
        );
        let before = d.clone();
        assert_eq!(ensure_core_font_metrics(&mut x, &mut d), Ok(None));
        assert_eq!(d.0.len(), before.0.len());
        assert_eq!(x.size, Some(1));
    }

    #[test]
    fn partial_metrics_are_an_error() {
        let mut x = table();
        let mut d = font("Helvetica", &[("FirstChar", Object::Integer(32))]);
        let err = ensure_core_font_metrics(&mut x, &mut d).unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot reconstruct partial standard-font metrics"),
            "{err}"
        );
    }

    #[test]
    fn other_fonts_and_subtypes_are_left_alone() {
        let mut x = table();
        let mut d = font("Arial", &[]);
        assert_eq!(ensure_core_font_metrics(&mut x, &mut d), Ok(None));
        let mut d = font("Helvetica", &[]);
        d.set(b"Subtype", Object::name("TrueType"));
        assert_eq!(ensure_core_font_metrics(&mut x, &mut d), Ok(None));
        assert!(!d.has_entry(b"Widths"));
    }

    #[test]
    fn a_differences_array_changes_the_widths() {
        let mut x = table();
        let enc = {
            let mut e = Dict::default();
            e.set(b"BaseEncoding", Object::name("WinAnsiEncoding"));
            e.set(
                b"Differences",
                Object::Array(vec![Object::Integer(65), Object::name("space")]),
            );
            e
        };
        let mut d = font("Helvetica", &[("Encoding", Object::Dict(enc))]);
        ensure_core_font_metrics(&mut x, &mut d).unwrap().unwrap();
        let w = ints(&d);
        assert_eq!(w[65], 278);
        assert_eq!(w[66], 667);
    }

    #[test]
    fn bad_encodings_and_differences_are_errors() {
        let mut x = table();
        let mut d = font(
            "Helvetica",
            &[("Encoding", Object::name("MacRomanEncoding"))],
        );
        let err = ensure_core_font_metrics(&mut x, &mut d).unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot reconstruct metrics for encoding MacRomanEncoding"),
            "{err}"
        );

        let mut d = font("Helvetica", &[("Encoding", Object::Integer(1))]);
        let err = ensure_core_font_metrics(&mut x, &mut d).unwrap_err();
        assert!(
            err.to_string().contains("invalid core font encoding"),
            "{err}"
        );

        for (diff, want) in [
            (vec![Object::Integer(300)], "code outside 0..255"),
            (vec![Object::name("A")], "name without valid code"),
            (
                vec![Object::Integer(65), Object::name("nosuchglyph")],
                "no metrics for glyph nosuchglyph",
            ),
            (
                vec![Object::Bool(true)],
                "invalid core font Differences entry",
            ),
        ] {
            let mut e = Dict::default();
            e.set(b"Differences", Object::Array(diff));
            let mut d = font("Helvetica", &[("Encoding", Object::Dict(e))]);
            let err = ensure_core_font_metrics(&mut x, &mut d).unwrap_err();
            assert!(err.to_string().contains(want), "{err}");
            assert!(!d.has_entry(b"Widths"));
        }
    }
}
