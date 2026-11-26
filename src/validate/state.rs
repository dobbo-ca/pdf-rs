//! The state of validation (docs/design.md section 2, `validate/`).
//!
//! [`ValidateState`] is what validation leaves on the document: Go keeps it as fields of
//! `XRefTable` (`Dests`, `CatalogXMPMeta`), and `XRefTable::vstate` holds it here. (`Names` is
//! `XRefTable::names`, which `model/names.rs` came with.)
//!
//! [`PassState`] is what Go carries in the `context.Context` of one validation pass (the
//! traversal records of properties, resources and patterns, the rendition page index). A package
//! that needs another field of either appends it here; nothing else in this module changes.

use super::page::ResourceTraversal;
use super::pattern::PatternTraversal;
use super::property::PropertiesTraversal;
use super::xmp::XmpMeta;
use crate::error::Error;
use crate::object::{Dict, Ref};
use std::collections::{BTreeMap, BTreeSet};

/// What validation records on the document.
#[derive(Debug, Clone, Default)]
pub struct ValidateState {
    /// `XRefTable.Dests`: the named destinations of the catalog.
    pub dests: Option<Dict>,
    /// `XRefTable.CatalogXMPMeta`: the parsed XMP packet of the catalog.
    pub catalog_xmp_meta: Option<XmpMeta>,
    /// `XRefTable.PageThumbs`: the thumbnail stream of each page that has one, by page number.
    pub page_thumbs: BTreeMap<i64, Ref>,
    /// `XRefTable.PageAnnots`: the annotations of each page by page number, built by the
    /// annotation validation and read by the form field validation.
    pub page_annots: BTreeMap<i64, crate::annotation::PgAnnots>,
    /// `XRefTable.Signatures`: the form signatures and the signatures located through page
    /// annotations only, by increment number and then object number.
    pub signatures: BTreeMap<i64, BTreeMap<i64, Signature>>,
}

/// `model.SigType*` (sign.go:47-51).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigType {
    Form,
    Page,
    Ur,
    Dts,
}

/// `model.Signature`: a digital signature found by the form field validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub sig_type: SigType,
    pub certified: bool,
    pub authoritative: bool,
    pub visible: bool,
    pub signed: bool,
    pub obj_nr: i64,
    pub page_nr: i64,
}

/// `renditionPageIndex` (rendition.go:24-31): the pages of the page tree and the rendition
/// references that have to be among them once the page walk is done.
#[derive(Debug, Clone, Default)]
pub struct RenditionPageIndex {
    pub pages: BTreeSet<i64>,
    pub pending: Vec<i64>,
    pub complete: bool,
    /// `annotations`: the annotation references of the pages, by page object number, cached for
    /// the rendition actions.
    pub annotations: std::collections::BTreeMap<i64, BTreeSet<crate::object::Ref>>,
}

impl RenditionPageIndex {
    /// `finish` (rendition.go:55-65): every pending page must be in the tree.
    ///
    /// # Errors
    /// [`Error::Cancelled`], and [`Error::Validation`] for a page that is not in the tree.
    pub fn finish(&mut self, cancel: &std::sync::atomic::AtomicBool) -> Result<(), Error> {
        for obj_nr in &self.pending {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !self.pages.contains(obj_nr) {
                return Err(Error::Validation(
                    "Rendition.AN.P: page is not in the page tree".to_string(),
                ));
            }
        }
        self.pending.clear();
        self.complete = true;
        Ok(())
    }
}

/// What Go keeps in the context of one validation pass.
#[derive(Debug, Clone)]
pub struct PassState {
    /// The depth of the resource traversal; -1 if there is none (`currentGraphDepths`).
    pub resource_depth: i64,
    /// The depth of the pattern traversal; 0 if there is none.
    pub pattern_depth: i64,
    /// The properties traversal (property.go), created by the outermost call.
    pub props: Option<PropertiesTraversal>,
    /// The pattern traversal (pattern.go), created by the outermost call. Its depth is
    /// `pattern_depth`.
    pub patterns: Option<PatternTraversal>,
    /// The resource traversal (page.go), created by the outermost call. Its depth is
    /// `resource_depth`.
    pub resources: Option<ResourceTraversal>,
    /// `withRenditionPageIndex`.
    pub rendition_pages: RenditionPageIndex,
    /// `withMediaValidation`: the 3D streams and rich media objects that validated, by role.
    pub media_validated: BTreeSet<(super::media::MediaValidationKind, crate::object::Ref)>,
    /// `withFormActionPages` (form_actions.go:37): the page numbers of the widgets of one AcroForm
    /// validation pass; `None` outside of it.
    pub form_action_pages: Option<super::action::FormActionPages>,
}

impl Default for PassState {
    fn default() -> PassState {
        PassState {
            resource_depth: -1,
            pattern_depth: 0,
            props: None,
            patterns: None,
            resources: None,
            rendition_pages: RenditionPageIndex::default(),
            media_validated: BTreeSet::new(),
            form_action_pages: None,
        }
    }
}
