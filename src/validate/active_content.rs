//! Active content notices (Go: validate/active_content.go).
//!
//! Not ported (docs/design.md section 1, row 13). Every function of the Go file only builds a
//! validation notice (`addActiveContentNotice`: `activeContentFinding`, `activeContentMessage`),
//! and notices are not ported: nothing observable depends on them, and nothing else reads them.
//! The validators that report a finding in Go (action.go, annotation.go, ...) simply do not.
//! The module stays declared in `validate/mod.rs` so the tree matches the design.
