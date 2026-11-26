//! Font names and the font resources of the interactive form (Go: font/fontDict.go `Name`,
//! primitives/font.go `FormFontResDict`).
//!
//! Names are byte strings: Go's `string` of a PDF name. Go's untyped errors are
//! [`Error::Corrupt`].

use crate::error::Error;
use crate::model::XRefTable;
use crate::object::Dict;

/// `font.Name`: the PostScript prefix (empty if there is none) and the name of the font
/// dictionary `font_dict`, which is object `obj_nr`. A Type 3 font without a `Name` is called
/// `Type3_<obj_nr>`.
///
/// # Errors
/// [`Error::Corrupt`] for a font dictionary without `Subtype`, or without `BaseFont` and `Name`
/// (or with a `Name` that is no name), and the errors of the lookups.
pub fn name(x: &mut XRefTable, font_dict: &Dict, obj_nr: i64) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let (subtype, _) = x
        .dereference_name_entry(font_dict, b"Subtype")
        .map_err(|e| e.context("fontName"))?;
    let subtype = match subtype {
        Some(s) if !s.is_empty() => s,
        _ => {
            return Err(Error::Corrupt(
                "fontName: missing fontDict entry \"Subtype\"".to_string(),
            ));
        }
    };

    let entry_name: &[u8] = if subtype == b"Type3" {
        // Type3 fonts only have Name in V1.0 else use generic name.
        if font_dict.find(b"Name").is_none() {
            return Ok((Vec::new(), format!("Type3_{obj_nr}").into_bytes()));
        }
        b"Name"
    } else if font_dict.find(b"BaseFont").is_some() {
        b"BaseFont"
    } else if font_dict.find(b"Name").is_some() {
        b"Name"
    } else {
        return Err(Error::Corrupt(
            "fontName: missing fontDict entries \"BaseFont\" and \"Name\"".to_string(),
        ));
    };

    let (base_font, _) = x
        .dereference_name_entry(font_dict, entry_name)
        .map_err(|e| e.context("fontName"))?;
    let Some(n) = base_font else {
        return Err(Error::Corrupt(
            "fontName: corrupt fontDict entry BaseFont".to_string(),
        ));
    };

    // Isolate Postscript prefix.
    if let Some(i) = n.iter().position(|&b| b == b'+')
        && i > 0
    {
        let prefix = n.get(..i).unwrap_or_default().to_vec();
        let rest = n.get(i + 1..).unwrap_or_default().to_vec();
        return Ok((prefix, rest));
    }

    Ok((Vec::new(), n))
}

/// `FormFontResDict`: the font resource dictionary of the default resources (`DR`) of the
/// interactive form. `None` if there is no form, no `DR`, no `Font` entry or an empty `DR`.
///
/// # Errors
/// The errors of the dereference, and [`Error::Corrupt`] for a `DR` or `Font` entry that is no
/// dictionary.
pub fn form_font_res_dict(x: &mut XRefTable) -> Result<Option<Dict>, Error> {
    let Some(o) = x
        .form
        .as_ref()
        .filter(|d| !d.is_empty())
        .and_then(|d| d.find(b"DR"))
        .cloned()
    else {
        return Ok(None);
    };

    let Some(res_dict) = x.dereference_dict(&o)?.filter(|d| !d.is_empty()).cloned() else {
        return Ok(None);
    };

    let Some(o) = res_dict.find(b"Font").cloned() else {
        return Ok(None);
    };

    Ok(x.dereference_dict(&o)?.cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::Entry;
    use crate::object::{Object, Ref};

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    #[test]
    fn the_name_of_a_font() {
        let mut x = XRefTable::new(Config::default());
        let d = dict(&[
            ("Subtype", Object::name("Type1")),
            ("BaseFont", Object::name("ABCDEF+Helvetica")),
        ]);
        let (p, n) = name(&mut x, &d, 7).unwrap();
        assert_eq!(
            (p.as_slice(), n.as_slice()),
            (&b"ABCDEF"[..], &b"Helvetica"[..])
        );

        // A leading plus is no prefix.
        let d = dict(&[
            ("Subtype", Object::name("TrueType")),
            ("Name", Object::name("+X")),
        ]);
        let (p, n) = name(&mut x, &d, 7).unwrap();
        assert_eq!((p.as_slice(), n.as_slice()), (&b""[..], &b"+X"[..]));

        // Type 3 fonts have a generic name unless they have a Name.
        let d = dict(&[("Subtype", Object::name("Type3"))]);
        let (p, n) = name(&mut x, &d, 7).unwrap();
        assert_eq!((p.as_slice(), n.as_slice()), (&b""[..], &b"Type3_7"[..]));
        let d = dict(&[
            ("Subtype", Object::name("Type3")),
            ("Name", Object::name("T3")),
            ("BaseFont", Object::name("Other")),
        ]);
        assert_eq!(name(&mut x, &d, 7).unwrap().1, b"T3");

        // Errors.
        assert!(name(&mut x, &Dict::default(), 7).is_err());
        let d = dict(&[("Subtype", Object::name("Type1"))]);
        assert!(name(&mut x, &d, 7).is_err());
        let d = dict(&[
            ("Subtype", Object::name("Type1")),
            ("BaseFont", Object::Null),
        ]);
        assert!(name(&mut x, &d, 7).is_err());
    }

    #[test]
    fn the_font_resources_of_the_form() {
        let mut x = XRefTable::new(Config::default());
        assert!(form_font_res_dict(&mut x).unwrap().is_none());

        let fonts = dict(&[(
            "F1",
            Object::Ref(Ref {
                num: 9,
                generation: 0,
            }),
        )]);
        x.table.insert(
            4,
            Entry::gen0(Object::Dict(dict(&[("Font", Object::Dict(fonts))]))),
        );
        x.form = Some(dict(&[(
            "DR",
            Object::Ref(Ref {
                num: 4,
                generation: 0,
            }),
        )]));
        let d = form_font_res_dict(&mut x).unwrap().unwrap();
        assert!(d.find(b"F1").is_some());

        x.form = Some(dict(&[("DR", Object::Dict(Dict::default()))]));
        assert!(form_font_res_dict(&mut x).unwrap().is_none());
        x.form = Some(dict(&[(
            "DR",
            Object::Dict(dict(&[("Other", Object::Null)])),
        )]));
        assert!(form_font_res_dict(&mut x).unwrap().is_none());
    }
}
