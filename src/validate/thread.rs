//! Threads and beads (Go: validate/thread.go, and `BeadVisit` of model/recursion.go:382).
//!
//! Differences from Go: validation is relaxed, so `P` of a bead is optional (a missing `P` only
//! yields a notice in Go, which is not ported), and a thread without `F` is accepted. The
//! `ValidationError` object numbers are dropped (see `object.rs`); the contexts name the bead
//! (`bead obj#N`) instead. Go repairs a null coordinate of a direct `R` array through the stored
//! map: `validate_bead_rectangle` writes the repair back to the bead dictionary.

use super::Validator;
use super::info::validate_document_info_dict;
use super::object::{
    OPTIONAL, REQUIRED, object_context, validate_ind_ref_entry, validate_name_entry,
    validate_rectangle_entry,
};
use crate::error::{Error, Sentinel};
use crate::model::deref::type_name;
use crate::model::{Version, XRefTable};
use crate::object::{Dict, Object, Ref};
use std::collections::BTreeSet;

/// `BeadVisit`: the bead objects of one chain that were entered. A bead that is entered twice is
/// a cycle. (Go never leaves a bead, so every bead of the chain counts.)
#[derive(Debug, Default)]
pub struct BeadVisit {
    ancestors: BTreeSet<i64>,
}

impl BeadVisit {
    /// `Enter`: object number 0 is not tracked.
    ///
    /// # Errors
    /// [`Sentinel::BeadCycle`] with the object number as context.
    fn enter(&mut self, obj_nr: i64) -> Result<(), Error> {
        if obj_nr == 0 {
            return Ok(());
        }
        if !self.ancestors.insert(obj_nr) {
            return Err(Error::Sentinel(Sentinel::BeadCycle).context(format!("obj#{obj_nr}")));
        }
        Ok(())
    }
}

/// The context of an error of one entry of a bead or thread: `role obj#N entry`.
fn entry_error(e: Error, role: &str, obj_nr: i64, entry: &str) -> Error {
    e.context(format!("{role} obj#{obj_nr} {entry}"))
}

/// A required reference entry (`validateIndRefEntry` with `REQUIRED`, which never returns nil).
fn required_ref(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    entry_name: &str,
    since: Version,
) -> Result<Ref, Error> {
    validate_ind_ref_entry(x, d, dict_name, entry_name, REQUIRED, since)?.ok_or_else(|| {
        Error::Validation(format!(
            "dict={dict_name} required entry={entry_name} missing"
        ))
    })
}

/// `validateRectangleEntry` for `R` of the bead `bead`, whose dictionary is `d`.
fn validate_bead_rectangle(
    x: &mut XRefTable,
    bead: Ref,
    d: &Dict,
    dict_name: &str,
    since: Version,
) -> Result<(), Error> {
    let a = validate_rectangle_entry(x, d, dict_name, "R", REQUIRED, since, None)?;
    // A direct array is repaired in place in Go; an indirect one the validator repaired itself.
    if let (Some(a), Some(Object::Array(_))) = (a, d.find(b"R"))
        && let Ok(Some(stored)) = x.dereference_dict_mut(bead)
        && let Some(slot) = stored.find_mut(b"R")
    {
        *slot = Object::Array(a);
    }
    Ok(())
}

/// `validateBeadPageEntry`: `P` refers to a page dictionary. Relaxed validation does not
/// require it.
fn validate_bead_page_entry(
    x: &mut XRefTable,
    d: &Dict,
    dict_name: &str,
    since: Version,
) -> Result<(), Error> {
    let Some(ir) = validate_ind_ref_entry(x, d, dict_name, "P", OPTIONAL, since)? else {
        return Ok(());
    };
    let Some(page) = x.dereference_dict_ref(ir)?.cloned() else {
        return Err(Error::Validation("missing page dict".to_string()));
    };
    validate_name_entry(
        x,
        &page,
        "pageDict",
        "Type",
        REQUIRED,
        Version::V10,
        Some(&|s: &[u8]| s == b"Page"),
    )?;
    Ok(())
}

/// `validateBeadDict`: the beads from `bead` on, up to the last bead `lbead`. `pbead` is the
/// bead before `bead`.
///
/// # Errors
/// [`Error::Cancelled`], [`Sentinel::BeadCycle`] for a bead that is visited twice,
/// [`Error::Validation`] for a bad bead, and the errors of the lookups.
pub fn validate_bead_dict(
    v: &mut Validator<'_>,
    bead: Ref,
    thread: Ref,
    pbead: Ref,
    lbead: Ref,
    visit: &mut BeadVisit,
) -> Result<(), Error> {
    let dict_name = "beadDict";
    let since = Version::V10;
    let mut bead = bead;
    let mut pbead = pbead;

    loop {
        let obj_nr = bead.num;
        v.check()?;
        visit.enter(obj_nr)?;

        let d =
            v.x.dereference_dict_ref(bead)
                .map_err(|e| e.context("bead: dereference dict"))?
                .cloned();
        let Some(d) = d else {
            return Err(Error::Validation("bead: missing dict".to_string()));
        };

        // Validate optional entry Type, must be "Bead".
        validate_name_entry(
            v.x,
            &d,
            dict_name,
            "Type",
            OPTIONAL,
            since,
            Some(&|s: &[u8]| s == b"Bead"),
        )
        .map_err(|e| entry_error(e, "bead", obj_nr, "Type"))?;

        // Validate entry T, must refer to threadDict.
        let t = validate_ind_ref_entry(v.x, &d, dict_name, "T", OPTIONAL, since)
            .map_err(|e| entry_error(e, "bead", obj_nr, "T"))?;
        if t.is_some_and(|t| t != thread) {
            return Err(Error::Validation(format!(
                "bead obj#{obj_nr}: invalid T backpointer to thread dict"
            )));
        }

        // Validate required entry R, must be rectangle.
        validate_bead_rectangle(v.x, bead, &d, dict_name, since)
            .map_err(|e| entry_error(e, "bead", obj_nr, "R"))?;

        // Validate required entry P, must be indRef to pageDict.
        validate_bead_page_entry(v.x, &d, dict_name, since)
            .map_err(|e| entry_error(e, "bead", obj_nr, "P"))?;

        // Validate required entry V, must refer to previous bead.
        let previous = required_ref(v.x, &d, dict_name, "V", since)
            .map_err(|e| e.context(format!("{dict_name}.V")))?;
        if previous != pbead {
            return Err(Error::Validation(format!(
                "bead obj#{obj_nr}: invalid V backpointer"
            )));
        }

        // Validate required entry N, must refer to last bead.
        let next = required_ref(v.x, &d, dict_name, "N", since)
            .map_err(|e| entry_error(e, "bead", obj_nr, "N"))?;

        if next == lbead {
            return Ok(());
        }

        pbead = bead;
        bead = next;
    }
}

/// `validateFirstBeadDict`: the first bead, and the chain it starts.
///
/// # Errors
/// As [`validate_bead_dict`].
pub fn validate_first_bead_dict(
    v: &mut Validator<'_>,
    bead: Ref,
    thread: Ref,
) -> Result<(), Error> {
    let dict_name = "firstBeadDict";
    let since = Version::V10;
    let obj_nr = bead.num;
    v.check()?;

    let d =
        v.x.dereference_dict_ref(bead)
            .map_err(|e| e.context("first bead: dereference dict"))?
            .cloned();
    let Some(d) = d else {
        return Err(Error::Validation("first bead: missing dict".to_string()));
    };

    validate_name_entry(
        v.x,
        &d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"Bead"),
    )
    .map_err(|e| entry_error(e, "first bead", obj_nr, "Type"))?;

    let t = required_ref(v.x, &d, dict_name, "T", since)
        .map_err(|e| entry_error(e, "first bead", obj_nr, "T"))?;
    if t != thread {
        return Err(Error::Validation(format!(
            "first bead obj#{obj_nr}: invalid T backpointer to thread dict"
        )));
    }

    validate_bead_rectangle(v.x, bead, &d, dict_name, since)
        .map_err(|e| entry_error(e, "first bead", obj_nr, "R"))?;

    validate_bead_page_entry(v.x, &d, dict_name, since)
        .map_err(|e| entry_error(e, "first bead", obj_nr, "P"))?;

    let pbead = required_ref(v.x, &d, dict_name, "V", since)
        .map_err(|e| entry_error(e, "first bead", obj_nr, "V"))?;
    let nbead = required_ref(v.x, &d, dict_name, "N", since)
        .map_err(|e| entry_error(e, "first bead", obj_nr, "N"))?;

    // If N and V reference this bead dict, it must be the first and only one.
    let sole = pbead == nbead && bead == pbead;
    if !sole {
        let intact = pbead != bead && nbead != bead;
        if !intact {
            return Err(Error::Validation(format!(
                "first bead obj#{obj_nr}: corrupt bead chain"
            )));
        }
        validate_bead_dict(v, nbead, thread, bead, pbead, &mut BeadVisit::default())
            .map_err(|e| e.context(format!("first bead obj#{obj_nr} next")))?;
    }
    Ok(())
}

/// `validateThreadDict`.
fn validate_thread_dict(v: &mut Validator<'_>, o: &Object, since: Version) -> Result<(), Error> {
    let dict_name = "threadDict";
    v.check()?;

    let Object::Ref(thread) = o else {
        return Err(Error::Validation(format!(
            "threadDict: expected indirect ref, got {}",
            type_name(o)
        )));
    };
    let obj_nr = thread.num;

    let d =
        v.x.dereference_dict_ref(*thread)
            .map_err(|e| e.context("thread: dereference dict"))?
            .cloned();
    let Some(d) = d else {
        return Err(Error::Validation("thread: missing dict".to_string()));
    };

    validate_name_entry(
        v.x,
        &d,
        dict_name,
        "Type",
        OPTIONAL,
        since,
        Some(&|s: &[u8]| s == b"Thread"),
    )
    .map_err(|e| entry_error(e, "thread", obj_nr, "Type"))?;

    // Validate optional thread information dict entry.
    if let Some(info) = d.find(b"I")
        && !info.is_null()
    {
        validate_document_info_dict(v.x, info)
            .map_err(|e| entry_error(e, "thread", obj_nr, "I"))?;
    }

    let f = validate_ind_ref_entry(v.x, &d, dict_name, "F", OPTIONAL, since)
        .map_err(|e| entry_error(e, "thread", obj_nr, "F"))?;
    let Some(f) = f else {
        // Relaxed: a thread without F is a spec violation, not an error.
        return Ok(());
    };

    // Validate the list of beads starting with the first bead dict.
    validate_first_bead_dict(v, f, *thread)
        .map_err(|e| e.context(format!("thread obj#{obj_nr} first bead")))
}

/// `validateThreads` (12.4.3 Articles): the `Threads` entry of the catalog.
///
/// # Errors
/// [`Error::Validation`] and the errors of the lookups.
pub fn validate_threads(
    v: &mut Validator<'_>,
    root: &mut Dict,
    required: bool,
    since: Version,
) -> Result<(), Error> {
    v.check()?;

    let Some(ir) = root.indirect_ref_entry(b"Threads") else {
        if required {
            return Err(Error::Validation(
                "rootDict.Threads: missing required entry".to_string(),
            ));
        }
        return Ok(());
    };

    let ro = Object::Ref(ir);
    let a =
        v.x.dereference_array(&ro)
            .map_err(|e| e.context("rootDict.Threads: dereference array"))?
            .cloned();
    let Some(a) = a else {
        return Ok(());
    };

    v.x.validate_version("threads", since)
        .map_err(|e| e.context("rootDict.Threads"))?;

    for (i, o) in a.iter().enumerate() {
        v.check()?;

        if o.is_null() {
            continue;
        }

        validate_thread_dict(v, o, since)
            .map_err(|e| e.context(object_context(&format!("rootDict.Threads[{i}]"), Some(o))))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::Entry;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn r(n: i64) -> Ref {
        Ref {
            num: n,
            generation: 0,
        }
    }

    /// `beadDict`.
    fn bead_dict(thread: Ref, previous: Ref, next: Ref) -> Dict {
        let mut d = Dict::default();
        d.set(b"T", Object::Ref(thread));
        d.set(
            b"R",
            Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(1),
                Object::Integer(1),
            ]),
        );
        d.set(b"V", Object::Ref(previous));
        d.set(b"N", Object::Ref(next));
        d
    }

    /// `beadXRefTable`.
    fn bead_table(max_depth: u64, dicts: Vec<(i64, Dict)>) -> XRefTable {
        let mut conf = Config::default();
        conf.limits.max_recursion_depth = max_depth;
        let mut x = XRefTable::new(conf);
        x.header_version = Some(Version::V17);
        for (n, d) in dicts {
            x.table.insert(n, Entry::gen0(Object::Dict(d)));
        }
        x
    }

    /// `TestValidateBeadDictAllowsLongChains`: a flat list is not recursive nesting.
    #[test]
    fn long_chains_are_allowed() {
        let bead_count = 102;
        let thread = r(10);
        let first = r(1);
        let mut dicts = Vec::new();
        for n in 1..=bead_count {
            let previous = if n == 1 { bead_count } else { n - 1 };
            let next = if n == bead_count { 1 } else { n + 1 };
            dicts.push((n, bead_dict(thread, r(previous), r(next))));
        }
        let mut x = bead_table(1, dicts);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        validate_first_bead_dict(&mut v, first, thread).unwrap();
    }

    /// `TestValidateBeadDictRejectsCycles`.
    #[test]
    fn cycles_are_rejected() {
        let thread = r(10);
        let first = r(1);
        let second = r(2);
        let third = r(3);
        let last = r(4);
        let cases = vec![
            // self reference
            vec![
                (1, bead_dict(thread, last, second)),
                (2, bead_dict(thread, first, second)),
            ],
            // multi-bead cycle
            vec![
                (1, bead_dict(thread, last, second)),
                (2, bead_dict(thread, first, third)),
                (3, bead_dict(thread, second, second)),
            ],
        ];
        for dicts in cases {
            let mut x = bead_table(100, dicts);
            let cancel = AtomicBool::new(false);
            let mut v = Validator::new(&mut x, &cancel);
            let err = validate_first_bead_dict(&mut v, first, thread).unwrap_err();
            assert!(err.is(Sentinel::BeadCycle), "got {err}");
        }
    }

    /// `TestValidateBeadDictAllowsValidCircularChains`.
    #[test]
    fn valid_circular_chains_are_allowed() {
        let thread = r(10);
        let first = r(1);
        let second = r(2);
        let cases = vec![
            // sole bead
            vec![(1, bead_dict(thread, first, first))],
            // two beads
            vec![
                (1, bead_dict(thread, second, second)),
                (2, bead_dict(thread, first, first)),
            ],
        ];
        for dicts in cases {
            let mut x = bead_table(100, dicts);
            let cancel = AtomicBool::new(false);
            let mut v = Validator::new(&mut x, &cancel);
            validate_first_bead_dict(&mut v, first, thread).unwrap();
        }
    }

    /// `TestValidateBeadDictCancellation`: the cancel flag stops the traversal.
    #[test]
    fn cancellation_interrupts_the_traversal() {
        let thread = r(10);
        let first = r(1);
        let mut dicts = Vec::new();
        for n in 1..=4 {
            let previous = if n == 1 { 4 } else { n - 1 };
            let next = if n == 4 { 1 } else { n + 1 };
            dicts.push((n, bead_dict(thread, r(previous), r(next))));
        }
        let mut x = bead_table(1, dicts);
        let cancel = AtomicBool::new(false);
        cancel.store(true, Ordering::Relaxed);
        let mut v = Validator::new(&mut x, &cancel);
        let err = validate_first_bead_dict(&mut v, first, thread).unwrap_err();
        assert!(matches!(err.root(), Error::Cancelled), "got {err}");
    }

    /// A direct `R` with a null coordinate is repaired in the stored dictionary.
    #[test]
    fn a_direct_rectangle_is_repaired_in_the_bead() {
        let thread = r(10);
        let first = r(1);
        let mut d = bead_dict(thread, first, first);
        d.set(
            b"R",
            Object::Array(vec![
                Object::Integer(0),
                Object::Null,
                Object::Integer(1),
                Object::Integer(1),
            ]),
        );
        let mut x = bead_table(100, vec![(1, d)]);
        let cancel = AtomicBool::new(false);
        let mut v = Validator::new(&mut x, &cancel);
        validate_first_bead_dict(&mut v, first, thread).unwrap();
        let stored = x.dereference_dict_ref(first).unwrap().cloned().unwrap();
        match stored.find(b"R") {
            Some(Object::Array(a)) => assert!(matches!(a[1], Object::Integer(0))),
            _ => panic!("array expected"),
        }
    }
}
