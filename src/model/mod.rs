//! The document model (docs/design.md section 2; Go: pkg/pdfcpu/model).
//!
//! * [`xref`]: the cross-reference table, its entries, the free list, the limit charges and the
//!   cumulative decode budget.
//! * [`deref`]: dereferencing, reference counts and the lazy decode of object-stream entries.
//! * [`context`]: `Context`, the read context and `Version`.
//! * [`names`]: the internalised name trees and `bind_name_trees`.
//! * [`pages`]: the page tree walk (`page_dict`, `ensure_page_count`) and its inherited attributes.
//! * [`equal`]: object equality (`equal_objects`).
//! * [`resources`]: `PageResourceNames`, page content, resource consolidation, `free_object` and
//!   `delete_object`.
//!
//! Each later package that belongs to the model adds its own `mod` line here.

pub mod context;
pub mod deref;
pub mod equal;
pub mod names;
pub mod pages;
pub mod resources;
pub mod xref;

pub use context::{Context, ReadInfo, Version};
pub use names::{NameEntry, Node};
pub use pages::{InheritedPageAttrs, PageTreeVisit};
pub use xref::{DecodedBudget, Enc, Entry, EntryObject, ParsedCounter, XRefTable};
