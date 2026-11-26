//! Document-level helpers (docs/design.md section 2, work package 26).
//!
//! * [`info`]: the document information dictionary (Go: pkg/pdfcpu/info.go, the part the port
//!   reaches).
//! * [`property`]: adding properties to the information dictionary (property.go).
//! * [`create`]: a new cross-reference table with a catalog, and a context for it
//!   (createContext.go).
//!
//! Not ported because byblos never reaches it: `PropertiesRemove`, `CreateContextWithXRefTable`,
//! and the PDF info listing (`Info`, `ListInfo`) of info.go.

pub mod create;
pub mod info;
pub mod property;
