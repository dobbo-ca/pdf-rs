//! The document information dictionary (Go: pkg/pdfcpu/info.go `ensureInfoDict`,
//! `handleInfoDict`, `extractAuthor`, `extractCreator`). `writeDocumentInfoDict` is in the
//! writer (`crate::write`).
//!
//! `Configuration.PreserveInfoDict` is fixed to false here (byblos never sets it), so the
//! producer and the dates are always stamped.

use crate::env::WriteEnv;
use crate::error::Error;
use crate::model::Context;
use crate::object::{Dict, Object};

/// `model.VersionStr`: the pdfcpu version that Go stamps into the producer.
const VERSION_STR: &str = "v0.16.4";

/// What `handleInfoDict` does for one entry of the info dictionary.
enum Action {
    /// `Author`: recorded on the table.
    Author(Object),
    /// `Creator`: recorded on the table.
    Creator(Object),
    /// `Producer`, `CreationDate` or `ModDate` that is an indirect reference: the object it
    /// refers to is redundant, because the entry is replaced by a direct string.
    Redundant(i64),
}

/// `CSVSafeString`.
fn csv_safe(s: &str) -> String {
    s.replace(';', ",")
}

/// `handleInfoDict`: records the author and the creator, and marks the objects of the entries
/// that are stamped anew as redundant. `d` is walked in key order (Go's map order is random).
///
/// # Errors
/// The errors of `dereference_text` for `Author` and `Creator`, and [`Error::Panic`] where Go
/// writes to the nil optimization context of a created document.
fn handle_info_dict(ctx: &mut Context, d: &Dict) -> Result<(), Error> {
    let mut actions = Vec::new();
    for (key, value) in d.iter() {
        match key.as_slice() {
            b"Author" => actions.push(Action::Author(value.clone())),
            b"Creator" => actions.push(Action::Creator(value.clone())),
            b"Producer" | b"CreationDate" | b"ModDate" => {
                // pdfcpu will modify these as direct dict entries.
                if let Object::Ref(r) = value {
                    // Get rid of these extra objects.
                    actions.push(Action::Redundant(r.num));
                }
            }
            _ => {}
        }
    }
    for a in actions {
        match a {
            Action::Author(v) => {
                // Record for stats.
                ctx.xref.author = csv_safe(&ctx.xref.dereference_text(&v)?);
            }
            Action::Creator(v) => {
                ctx.xref.creator = csv_safe(&ctx.xref.dereference_text(&v)?);
            }
            Action::Redundant(n) => match ctx.optimize.as_mut() {
                Some(o) => {
                    o.duplicate_info_objs.insert(n);
                }
                None => return Err(Error::Panic("nil optimization context")),
            },
        }
    }
    Ok(())
}

/// `ensureInfoDict`: makes sure the document has an information dictionary, and stamps its
/// producer and dates from `env.now`.
///
/// A document without one gets a new dictionary with all three. One with a dictionary has the
/// three entries replaced (14.3.3 Document Information Dictionary).
///
/// # Errors
/// [`Error::Panic`] where Go dereferences a nil, the errors of the dereference of the
/// dictionary, and those of `handle_info_dict`.
pub fn ensure_info_dict(ctx: &mut Context, env: &WriteEnv) -> Result<(), Error> {
    let now = env.now.date_string();
    let producer = format!("pdfcpu {VERSION_STR}");

    let Some(info) = ctx.xref.info else {
        let mut d = Dict::default();
        d.insert_string(b"Producer", producer.as_bytes());
        d.insert_string(b"CreationDate", now.as_bytes());
        d.insert_string(b"ModDate", now.as_bytes());
        let ir = ctx.xref.ind_ref_for_new_object(Object::Dict(d))?;
        ctx.xref.info = Some(ir);
        return Ok(());
    };

    let Some(d) = ctx.xref.dereference_dict_ref(info)?.cloned() else {
        return Ok(());
    };
    handle_info_dict(ctx, &d)?;

    if let Some(d) = ctx.xref.dereference_dict_mut(info)? {
        d.update(b"CreationDate", Object::string(now.as_bytes()));
        d.update(b"ModDate", Object::string(now.as_bytes()));
        d.update(b"Producer", Object::string(producer.as_bytes()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::Version;
    use crate::object::Ref;

    fn ctx() -> Context {
        let mut ctx = Context::new(0, Config::default()).unwrap();
        ctx.xref.header_version = Some(Version::V17);
        ctx.xref.table.insert(0, crate::model::Entry::free_head());
        ctx.xref.size = Some(1);
        ctx
    }

    fn info_dict(ctx: &mut Context) -> Dict {
        let info = ctx.xref.info.unwrap();
        ctx.xref
            .dereference_dict_ref(info)
            .unwrap()
            .unwrap()
            .clone()
    }

    // Port of TestEnsureInfoDictPreserveModeStillCreatesMissingDictionary, without the
    // preserve mode (never set by byblos).
    #[test]
    fn creates_a_missing_dictionary() {
        let mut ctx = ctx();
        ensure_info_dict(&mut ctx, &WriteEnv::parity()).unwrap();
        let d = info_dict(&mut ctx);
        assert_eq!(
            d.string_entry(b"Producer"),
            Some(b"pdfcpu v0.16.4".as_slice())
        );
        for k in [b"CreationDate".as_slice(), b"ModDate"] {
            assert_eq!(
                d.string_entry(k),
                Some(b"D:20000101000000+00'00'".as_slice())
            );
        }
        assert_eq!(ctx.xref.size, Some(2));
    }

    // Port of the default-stamping case of TestEnsureInfoDictPreservesIndirectEntries: the
    // entries become direct strings and the objects they referred to are redundant.
    #[test]
    fn stamps_and_marks_indirect_entries_redundant() {
        let mut ctx = ctx();
        let mut refs = Vec::new();
        for s in ["producer", "creation-date", "mod-date"] {
            refs.push(ctx.xref.ind_ref_for_new_object(Object::string(s)).unwrap());
        }
        let mut d = Dict::default();
        d.set(b"Producer", Object::Ref(refs[0]));
        d.set(b"CreationDate", Object::Ref(refs[1]));
        d.set(b"ModDate", Object::Ref(refs[2]));
        d.set(b"Author", Object::string("a;b"));
        ctx.xref.info = Some(ctx.xref.ind_ref_for_new_object(Object::Dict(d)).unwrap());

        ensure_info_dict(&mut ctx, &WriteEnv::parity()).unwrap();

        let d = info_dict(&mut ctx);
        assert_eq!(
            d.string_entry(b"Producer"),
            Some(b"pdfcpu v0.16.4".as_slice())
        );
        assert!(matches!(d.find(b"CreationDate"), Some(Object::String(_))));
        assert!(matches!(d.find(b"ModDate"), Some(Object::String(_))));
        let opt = ctx.optimize.as_ref().unwrap();
        for r in &refs {
            assert!(opt.duplicate_info_objs.contains(&r.num), "{r:?}");
        }
        assert_eq!(ctx.xref.author, "a,b");
    }

    #[test]
    fn a_created_document_has_no_optimization_context_to_mark() {
        let mut ctx = ctx();
        ctx.optimize = None;
        let r = ctx
            .xref
            .ind_ref_for_new_object(Object::string("p"))
            .unwrap();
        let mut d = Dict::default();
        d.set(b"Producer", Object::Ref(r));
        ctx.xref.info = Some(ctx.xref.ind_ref_for_new_object(Object::Dict(d)).unwrap());
        let err = ensure_info_dict(&mut ctx, &WriteEnv::parity()).unwrap_err();
        assert!(matches!(err, Error::Panic(_)), "{err}");
    }

    #[test]
    fn an_info_that_is_not_a_dictionary_is_an_error() {
        let mut ctx = ctx();
        ctx.xref.info = Some(ctx.xref.ind_ref_for_new_object(Object::Integer(1)).unwrap());
        let err = ensure_info_dict(&mut ctx, &WriteEnv::parity()).unwrap_err();
        assert!(err.is(crate::error::Sentinel::ExpectedDict), "{err}");
    }

    #[test]
    fn a_dangling_info_is_left_alone() {
        let mut ctx = ctx();
        ctx.xref.info = Some(Ref {
            num: 9,
            generation: 0,
        });
        assert_eq!(ensure_info_dict(&mut ctx, &WriteEnv::parity()), Ok(()));
    }
}
