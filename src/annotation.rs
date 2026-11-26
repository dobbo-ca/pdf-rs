//! The annotation records validation leaves on the document (Go: pkg/pdfcpu/annotation.go
//! `Annotation` and `linkAnnotation`, pkg/pdfcpu/model/annotation.go `AnnotationType`,
//! `Annotation`, `PgAnnots`).
//!
//! Go builds an `AnnotationRenderer` per page annotation during validation and caches it in
//! `XRefTable.PageAnnots` (kept in [`ValidateState::page_annots`](crate::validate::state)). The
//! form field validation is the only reader and uses the type, the rectangle string and the
//! appearance object number; the other fields of Go's renderer (contents, `NM`, flags, colour,
//! border radii) are read by nothing the port reaches, so they are not kept. Parsing them still
//! happens in [`annotation`], because a malformed value is an error there.
//!
//! Go's renderer types for `Text`, `Link` and `Popup` (and the raw type for the rest) all
//! answer the same four questions, so one struct stands for all of them.

use crate::error::Error;
use crate::model::{Version, XRefTable};
use crate::object::{Dict, Object, Rect, Ref};
use std::collections::BTreeMap;

/// `model.AnnotationType`. An unknown subtype is [`AnnotationType::Custom`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AnnotationType {
    Text,
    Link,
    FreeText,
    Line,
    Square,
    Circle,
    Polygon,
    PolyLine,
    Highlight,
    Underline,
    Squiggly,
    StrikeOut,
    Stamp,
    Caret,
    Ink,
    Popup,
    FileAttachment,
    Sound,
    Movie,
    Widget,
    Screen,
    PrinterMark,
    TrapNet,
    Watermark,
    /// Go's `Ann3D`.
    ThreeD,
    Redact,
    Custom,
}

impl AnnotationType {
    /// `AnnotTypes[subtype]`, with `Custom` for a subtype that is not in the table.
    pub fn from_subtype(subtype: &[u8]) -> AnnotationType {
        match subtype {
            b"Text" => AnnotationType::Text,
            b"Link" => AnnotationType::Link,
            b"FreeText" => AnnotationType::FreeText,
            b"Line" => AnnotationType::Line,
            b"Square" => AnnotationType::Square,
            b"Circle" => AnnotationType::Circle,
            b"Polygon" => AnnotationType::Polygon,
            b"PolyLine" => AnnotationType::PolyLine,
            b"Highlight" => AnnotationType::Highlight,
            b"Underline" => AnnotationType::Underline,
            b"Squiggly" => AnnotationType::Squiggly,
            b"StrikeOut" => AnnotationType::StrikeOut,
            b"Stamp" => AnnotationType::Stamp,
            b"Caret" => AnnotationType::Caret,
            b"Ink" => AnnotationType::Ink,
            b"Popup" => AnnotationType::Popup,
            b"FileAttachment" => AnnotationType::FileAttachment,
            b"Sound" => AnnotationType::Sound,
            b"Movie" => AnnotationType::Movie,
            b"Widget" => AnnotationType::Widget,
            b"Screen" => AnnotationType::Screen,
            b"PrinterMark" => AnnotationType::PrinterMark,
            b"TrapNet" => AnnotationType::TrapNet,
            b"Watermark" => AnnotationType::Watermark,
            b"3D" => AnnotationType::ThreeD,
            b"Redact" => AnnotationType::Redact,
            _ => AnnotationType::Custom,
        }
    }
}

/// The part of Go's `model.AnnotationRenderer` that is read after validation.
#[derive(Debug, Clone, PartialEq)]
pub struct Annotation {
    /// `Type()`.
    pub kind: AnnotationType,
    /// `Rectangle()`; see [`rect_short_string`] for `RectString()`.
    pub rect: Rect,
    /// `APObjNrInt()`: the object number of the appearance dictionary, 0 if it is not a
    /// reference.
    pub ap_obj_nr: i64,
}

impl Annotation {
    /// `RectString`.
    pub fn rect_string(&self) -> String {
        rect_short_string(&self.rect)
    }
}

/// `model.Annot`: the annotations of one type on one page.
#[derive(Debug, Clone, Default)]
pub struct Annot {
    /// `IndRefs`: the references of the annotations that are indirect, in page order.
    pub ind_refs: Vec<Ref>,
    /// `Map`: by object number, or by the negated array index for a direct annotation.
    pub map: BTreeMap<i64, Annotation>,
}

/// `model.PgAnnots`: the annotations of one page by type.
pub type PgAnnots = BTreeMap<AnnotationType, Annot>;

/// `Rectangle.ShortString`.
pub fn rect_short_string(r: &Rect) -> String {
    format!(
        "({:3.0}, {:3.0}, {:3.0}, {:3.0})",
        r.ll.x, r.ll.y, r.ur.x, r.ur.y
    )
}

/// `XRefTable.RectForArray`: the rectangle of the four numbers `a`, each dereferenced.
///
/// # Errors
/// [`Error::Panic`] where Go indexes past the end of an array with fewer than four elements,
/// and the errors of `XRefTable::dereference_number`.
pub fn rect_for_array(x: &mut XRefTable, a: &[Object]) -> Result<Rect, Error> {
    let [llx, lly, urx, ury, ..] = a else {
        return Err(Error::Panic("index out of range in RectForArray"));
    };
    let llx = x.dereference_number(llx)?;
    let lly = x.dereference_number(lly)?;
    let urx = x.dereference_number(urx)?;
    let ury = x.dereference_number(ury)?;
    Ok(Rect::new(llx, lly, urx, ury))
}

/// `annotationRendererRect`: the `Rect` of the annotation; a zero rectangle where it is missing
/// or malformed (relaxed validation).
fn annotation_rect(x: &mut XRefTable, d: &Dict) -> Result<Rect, Error> {
    let arr = match d.find(b"Rect") {
        Some(o) => x.dereference_array(o)?.cloned(),
        None => None,
    };
    match arr {
        Some(a) if a.len() == 4 => rect_for_array(x, &a),
        _ => Ok(Rect::new(0.0, 0.0, 0.0, 0.0)),
    }
}

/// `linkAnnotation`: only the lookups that can fail. Relaxed validation skips an action that
/// does not dereference.
fn check_link_action(x: &mut XRefTable, d: &Dict) -> Result<(), Error> {
    let Some(o) = d.find(b"A").filter(|o| !o.is_null()) else {
        return Ok(());
    };
    let action_obj_nr = match o {
        Object::Ref(r) => r.num,
        _ => 0,
    };
    let Some(action) = x.dereference_dict(o).ok().flatten().cloned() else {
        return Ok(());
    };
    x.dereference_string_entry_bytes(&action, b"URI")
        .map_err(|e| {
            if action_obj_nr > 0 {
                e.context(format!("link annotation action obj#{action_obj_nr}"))
            } else {
                e.context("link annotation action")
            }
        })?;
    Ok(())
}

/// `Annotation`: the renderer of the annotation dictionary `d`.
///
/// # Errors
/// [`Error::Validation`] for a missing `Subtype`, and the errors of the lookups of the
/// `Subtype`, `Rect`, `Contents`, `NM` and `F` entries (and of the `A` and `URI` entries of a
/// link).
pub fn annotation(x: &mut XRefTable, d: &Dict) -> Result<Annotation, Error> {
    let (subtype, _) = x
        .dereference_name_entry(d, b"Subtype")
        .map_err(|e| e.context("annotation entry Subtype"))?;
    let Some(subtype) = subtype else {
        return Err(Error::Validation("annotation: missing Subtype".to_string()));
    };

    let rect = annotation_rect(x, d)?;

    let ap_obj_nr = d.indirect_ref_entry(b"AP").map_or(0, |r| r.num);

    if let Some(c) = d.find(b"Contents") {
        x.dereference_string_or_hex_literal(c, Version::V10, None)?;
    }

    // `NM`: what pdfcpu refers to as the annotation id.
    x.dereference_string_entry(d, b"NM")
        .map_err(|e| e.context("annotation entry NM"))?;

    x.dereference_integer_entry(d, b"F")
        .map_err(|e| e.context("annotation entry F"))?;

    if subtype == b"Link" {
        check_link_action(x, d)?;
    }

    Ok(Annotation {
        kind: AnnotationType::from_subtype(&subtype),
        rect,
        ap_obj_nr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::object::array::new_number_array;

    fn table() -> XRefTable {
        let mut x = XRefTable::new(Config::default());
        x.header_version = Some(Version::V17);
        x
    }

    #[test]
    fn types_by_subtype() {
        assert_eq!(
            AnnotationType::from_subtype(b"Widget"),
            AnnotationType::Widget
        );
        assert_eq!(AnnotationType::from_subtype(b"3D"), AnnotationType::ThreeD);
        assert_eq!(
            AnnotationType::from_subtype(b"Custom"),
            AnnotationType::Custom
        );
        assert_eq!(
            AnnotationType::from_subtype(b"Nope"),
            AnnotationType::Custom
        );
    }

    #[test]
    fn short_string_pads_to_three() {
        let r = Rect::new(0.0, 12.4, 612.0, 1000.0);
        assert_eq!(rect_short_string(&r), "(  0,  12, 612, 1000)");
    }

    #[test]
    fn renderer_of_a_widget() {
        let mut x = table();
        let mut d = Dict::default();
        d.set(b"Subtype", Object::name("Widget"));
        d.set(
            b"Rect",
            Object::Array(new_number_array(&[1.0, 2.0, 3.0, 4.0])),
        );
        d.set(
            b"AP",
            Object::Ref(Ref {
                num: 7,
                generation: 0,
            }),
        );
        d.set(b"Contents", Object::string("hello"));
        let ann = annotation(&mut x, &d).unwrap();
        assert_eq!(ann.kind, AnnotationType::Widget);
        assert_eq!(ann.ap_obj_nr, 7);
        assert_eq!(ann.rect, Rect::new(1.0, 2.0, 3.0, 4.0));
        assert_eq!(ann.rect_string(), "(  1,   2,   3,   4)");
    }

    #[test]
    fn a_missing_rect_is_zero_and_a_missing_subtype_is_an_error() {
        let mut x = table();
        let mut d = Dict::default();
        assert!(matches!(annotation(&mut x, &d), Err(Error::Validation(_))));
        d.set(b"Subtype", Object::name("Foo"));
        let ann = annotation(&mut x, &d).unwrap();
        assert_eq!(ann.kind, AnnotationType::Custom);
        assert_eq!(ann.rect, Rect::new(0.0, 0.0, 0.0, 0.0));
        assert_eq!(ann.ap_obj_nr, 0);
    }
}
