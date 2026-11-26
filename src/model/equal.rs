//! Object equality (Go: pkg/pdfcpu/model/equal.go). [`equal_objects`] compares two objects
//! that may be object trees, following references in the table.
//!
//! The comparison is bounded twice, and a comparison that goes over either bound is not an
//! error: the objects are treated as unequal. The recursion depth is the configured
//! `max_recursion_depth`. The number of pairs of different references that are compared is the
//! size of the table, which is enough for graphs that have the same shape.
//!
//! Differences from Go:
//!
//! * Go's `errObjectComparisonLimit` is the private `Cmp::Limit`; it carries no error, so
//!   nothing can wrap it and it reaches [`equal_objects`] unchanged.
//! * A reference is resolved to a copy of its target, because the table is borrowed for the
//!   next lookup (every comparison step holds `&mut XRefTable`).
//! * The set of compared pairs is charged to the ledger for the comparison, `PAIR_BYTES` per
//!   pair.
//! * Go's `log.Optimize` diagnostics are not ported.

use super::deref::type_name;
use super::resources::check_cancel;
use super::xref::XRefTable;
use crate::error::Error;
use crate::ledger::Held;
use crate::object::{Dict, Object, StreamDict};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::mem::discriminant;
use std::sync::atomic::AtomicBool;

/// One pair in [`Comparator::pairs`], in B-tree nodes about half full.
const PAIR_BYTES: u64 = 2 * size_of::<(i64, i64)>() as u64;

/// What `equalStreamBytes` compares between two cancellation checks.
const CHUNK_SIZE: usize = 64 * 1024;

/// Why a comparison stopped: Go's `errObjectComparisonLimit`, or an error.
enum Cmp {
    Limit,
    Fail(Error),
}

impl From<Error> for Cmp {
    fn from(e: Error) -> Cmp {
        Cmp::Fail(e)
    }
}

impl Cmp {
    /// `fmt.Errorf("msg: %w", err)`; the limit has nothing to wrap.
    fn context(self, msg: String) -> Cmp {
        match self {
            Cmp::Limit => Cmp::Limit,
            Cmp::Fail(e) => Cmp::Fail(e.context(msg)),
        }
    }
}

/// `newObjectPair`: the object numbers in order.
fn new_object_pair(a: i64, b: i64) -> (i64, i64) {
    if a > b { (b, a) } else { (a, b) }
}

struct Comparator<'a> {
    x: &'a mut XRefTable,
    cancel: &'a AtomicBool,
    pairs: BTreeSet<(i64, i64)>,
    /// The charge of `pairs`.
    held: Held,
    pair_count: usize,
    pair_limit: usize,
}

/// `EqualObjects`: whether `o1` and `o2` are equal in the context of the table `x`. An object
/// and a reference to it are equal. Going over the recursion depth or over the budget of
/// reference pairs (the size of the table) gives `false`, not an error. `pairs` holds pairs of
/// object numbers (two entries each) that the caller is comparing already; meeting one of them
/// again counts as equal.
///
/// # Errors
/// [`Error::Cancelled`]; a dereferencing error; `stream dict not loaded` for equal stream
/// dictionaries when one has no raw bytes; `unhandled compare for type ...`.
pub fn equal_objects(
    x: &mut XRefTable,
    o1: &Object,
    o2: &Object,
    pairs: &[i64],
    cancel: &AtomicBool,
) -> Result<bool, Error> {
    // Equal objects with isomorphic reference graphs need at most one pair per object. Larger
    // comparisons are treated as unequal.
    let pair_limit = x.table.len().max(1);
    let held = x.mem().hold();
    let mut oc = Comparator {
        x,
        cancel,
        pairs: BTreeSet::new(),
        held,
        pair_count: 0,
        pair_limit,
    };
    let mut i = 0;
    while i + 1 < pairs.len() {
        check_cancel(cancel)?;
        oc.held.grow_by(PAIR_BYTES)?;
        oc.pairs.insert(new_object_pair(pairs[i], pairs[i + 1]));
        i += 2;
    }
    match oc.equal_objects(o1, o2, 0) {
        Ok(ok) => Ok(ok),
        Err(Cmp::Limit) => Ok(false),
        Err(Cmp::Fail(e)) => Err(e),
    }
}

impl Comparator<'_> {
    /// `Dereference` of `o`: a reference gives a copy of its target.
    fn resolve<'o>(&mut self, o: &'o Object) -> Result<Cow<'o, Object>, Error> {
        match o {
            Object::Ref(_) => Ok(Cow::Owned(self.x.deref_c(o, self.cancel)?.clone())),
            _ => Ok(Cow::Borrowed(o)),
        }
    }

    fn equal_objects(&mut self, o1: &Object, o2: &Object, depth: u64) -> Result<bool, Cmp> {
        check_cancel(self.cancel)?;

        if self
            .x
            .check_recursion_depth("object comparison", depth)
            .is_err()
        {
            return Err(Cmp::Limit);
        }

        if self.equal_object_references(o1, o2)? {
            return Ok(true);
        }

        let a = self.resolve(o1)?;
        let b = self.resolve(o2)?;
        let (a, b) = (&*a, &*b);

        if discriminant(a) != discriminant(b) {
            return Ok(false);
        }

        Ok(match (a, b) {
            (Object::Null, Object::Null) => true,
            (Object::Name(x), Object::Name(y)) => x == y,
            (Object::String(x), Object::String(y)) => x == y,
            (Object::Hex(x), Object::Hex(y)) => x == y,
            (Object::Integer(x), Object::Integer(y)) => x == y,
            (Object::Float(x), Object::Float(y)) => x == y,
            (Object::Bool(x), Object::Bool(y)) => x == y,
            (Object::Dict(x), Object::Dict(y)) => self.equal_dicts(x, y, depth)?,
            (Object::Stream(x), Object::Stream(y)) => self.equal_stream_dicts(x, y, depth)?,
            (Object::Array(x), Object::Array(y)) => self.equal_arrays(x, y, depth)?,
            (x, _) => {
                return Err(Cmp::Fail(Error::Corrupt(format!(
                    "unhandled compare for type {}",
                    type_name(x)
                ))));
            }
        })
    }

    /// `equalObjectReferences`: two references are equal if they are the same, or if their
    /// pair is being compared already. A new pair is recorded and gives `false`.
    fn equal_object_references(&mut self, o1: &Object, o2: &Object) -> Result<bool, Cmp> {
        let (Object::Ref(r1), Object::Ref(r2)) = (o1, o2) else {
            return Ok(false);
        };
        if r1 == r2 {
            return Ok(true);
        }

        let pair = new_object_pair(r1.num, r2.num);
        if self.pairs.contains(&pair) {
            return Ok(true);
        }

        if self.pair_count >= self.pair_limit {
            return Err(Cmp::Limit);
        }

        self.held.grow_by(PAIR_BYTES)?;
        self.pairs.insert(pair);
        self.pair_count += 1;
        Ok(false)
    }

    fn equal_arrays(&mut self, a1: &[Object], a2: &[Object], depth: u64) -> Result<bool, Cmp> {
        check_cancel(self.cancel)?;

        if a1.len() != a2.len() {
            return Ok(false);
        }

        for (o1, o2) in a1.iter().zip(a2) {
            if !self.equal_objects(o1, o2, depth + 1)? {
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// `equalStreamDicts`: equal dictionaries and the same raw bytes.
    fn equal_stream_dicts(
        &mut self,
        sd1: &StreamDict,
        sd2: &StreamDict,
        depth: u64,
    ) -> Result<bool, Cmp> {
        if !self.equal_dicts(&sd1.dict, &sd2.dict, depth)? {
            return Ok(false);
        }

        let (Some(a), Some(b)) = (&sd1.raw, &sd2.raw) else {
            return Err(Cmp::Fail(Error::Corrupt(
                "stream dict not loaded".to_string(),
            )));
        };

        Ok(equal_stream_bytes(self.cancel, a, b)?)
    }

    /// `equalFontNames`: the names, without the subset prefix.
    fn equal_font_names(&mut self, v1: &Object, v2: &Object) -> Result<bool, Error> {
        let bf1 = self.font_name(v1)?;
        let bf2 = self.font_name(v2)?;
        Ok(strip_font_prefix(&bf1) == strip_font_prefix(&bf2))
    }

    fn font_name(&mut self, v: &Object) -> Result<Vec<u8>, Error> {
        match self.x.deref_c(v, self.cancel)? {
            Object::Name(n) => Ok(n.clone()),
            _ => Err(Error::Corrupt("type cast problem".to_string())),
        }
    }

    /// `fontDictPair`: whether both dictionaries have the `/Type /Font`.
    fn font_dict_pair(&mut self, d1: &Dict, d2: &Dict) -> Result<bool, Error> {
        let (t1, _) = self
            .x
            .dereference_name_entry(d1, b"Type")
            .map_err(|e| e.context("first dict Type"))?;
        let (t2, _) = self
            .x
            .dereference_name_entry(d2, b"Type")
            .map_err(|e| e.context("second dict Type"))?;
        Ok(t1.as_deref() == Some(b"Font".as_slice()) && t2.as_deref() == Some(b"Font".as_slice()))
    }

    fn equal_dicts(&mut self, d1: &Dict, d2: &Dict, depth: u64) -> Result<bool, Cmp> {
        check_cancel(self.cancel)?;

        if d1.len() != d2.len() {
            return Ok(false);
        }

        let font_dicts = self.font_dict_pair(d1, d2)?;

        // The keys are in sorted order, so the error of the lowest key is the one reported.
        for (key, v1) in d1.iter() {
            check_cancel(self.cancel)?;

            let Some(v2) = d2.0.get(key) else {
                return Ok(false);
            };

            let entry = || format!("dict entry {}", String::from_utf8_lossy(key));

            // Special treatment for font dicts.
            if font_name_entry(font_dicts, key) {
                if !self
                    .equal_font_names(v1, v2)
                    .map_err(|e| Cmp::Fail(e.context(entry())))?
                {
                    return Ok(false);
                }
                continue;
            }

            if !self
                .equal_objects(v1, v2, depth + 1)
                .map_err(|e| e.context(entry()))?
            {
                return Ok(false);
            }
        }

        Ok(true)
    }
}

/// `fontNameEntry`.
fn font_name_entry(font_dicts: bool, key: &[u8]) -> bool {
    font_dicts && matches!(key, b"BaseFont" | b"FontName" | b"Name")
}

/// The name without the prefix up to the first `+` (a `+` at the start is no prefix).
fn strip_font_prefix(n: &[u8]) -> &[u8] {
    match n.iter().position(|&c| c == b'+') {
        Some(i) if i > 0 => &n[i + 1..],
        _ => n,
    }
}

/// `equalStreamBytes`: compares in chunks, so a cancellation is seen within one chunk.
fn equal_stream_bytes(cancel: &AtomicBool, a: &[u8], b: &[u8]) -> Result<bool, Error> {
    check_cancel(cancel)?;

    if a.len() != b.len() {
        return Ok(false);
    }

    for (ca, cb) in a.chunks(CHUNK_SIZE).zip(b.chunks(CHUNK_SIZE)) {
        check_cancel(cancel)?;
        if ca != cb {
            return Ok(false);
        }
    }

    check_cancel(cancel)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Limits};
    use crate::model::xref::{Entry, EntryObject};
    use crate::object::Ref;

    fn r(n: i64) -> Ref {
        Ref {
            num: n,
            generation: 0,
        }
    }

    fn dict(entries: &[(&str, Object)]) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v.clone());
        }
        d
    }

    fn eq(x: &mut XRefTable, a: &Object, b: &Object, pairs: &[i64]) -> Result<bool, Error> {
        equal_objects(x, a, b, pairs, &AtomicBool::new(false))
    }

    /// Port of ghsa8pv5SharedChains: two chains of `depth` arrays that hold the next link twice.
    fn shared_chains(depth: i64, unequal: bool) -> (XRefTable, Object, Object) {
        let mut x = XRefTable::new(Config::default());
        let x_base = 100;
        let y_base = x_base + depth + 1;
        for i in 0..depth {
            let x_next = Object::Ref(r(x_base + i + 1));
            let y_next = Object::Ref(r(y_base + i + 1));
            x.table.insert(
                x_base + i,
                Entry::gen0(Object::Array(vec![x_next.clone(), x_next])),
            );
            x.table.insert(
                y_base + i,
                Entry::gen0(Object::Array(vec![y_next.clone(), y_next])),
            );
        }
        x.table
            .insert(x_base + depth, Entry::gen0(Object::Integer(1)));
        let y_leaf = if unequal { 2 } else { 1 };
        x.table
            .insert(y_base + depth, Entry::gen0(Object::Integer(y_leaf)));
        (x, Object::Ref(r(x_base)), Object::Ref(r(y_base)))
    }

    // Port of TestEqualObjectsRejectsUnloadedSecondStream.
    #[test]
    fn rejects_an_unloaded_second_stream() {
        let mut x = XRefTable::new(Config::default());
        let loaded = StreamDict {
            raw: Some(x.mem().bytes_from(b"loaded", 6).unwrap()),
            ..StreamDict::default()
        };
        let unloaded = StreamDict::default();
        let err = eq(
            &mut x,
            &Object::Stream(Box::new(loaded)),
            &Object::Stream(Box::new(unloaded)),
            &[],
        )
        .unwrap_err();
        assert_eq!(err, Error::Corrupt("stream dict not loaded".to_string()));
    }

    #[test]
    fn streams_compare_their_dicts_and_raw_bytes() {
        let mut x = XRefTable::new(Config::default());
        let mem = x.mem().clone();
        let stream = |len: i64, raw: &[u8]| {
            Object::Stream(Box::new(StreamDict {
                dict: dict(&[("Length", Object::Integer(len))]),
                raw: Some(mem.bytes_from(raw, raw.len()).unwrap()),
                ..StreamDict::default()
            }))
        };
        assert_eq!(
            eq(&mut x, &stream(3, b"abc"), &stream(3, b"abc"), &[]),
            Ok(true)
        );
        assert_eq!(
            eq(&mut x, &stream(3, b"abc"), &stream(3, b"abd"), &[]),
            Ok(false)
        );
        assert_eq!(
            eq(&mut x, &stream(3, b"abc"), &stream(4, b"abc"), &[]),
            Ok(false)
        );
        // Different lengths are unequal without a chunk comparison.
        assert_eq!(
            eq(&mut x, &stream(3, b"abc"), &stream(3, b"abcd"), &[]),
            Ok(false)
        );
        // More than one chunk.
        let big = vec![7u8; CHUNK_SIZE * 2 + 5];
        let mut other = big.clone();
        assert_eq!(
            eq(&mut x, &stream(1, &big), &stream(1, &other), &[]),
            Ok(true)
        );
        other[CHUNK_SIZE * 2 + 4] = 8;
        assert_eq!(
            eq(&mut x, &stream(1, &big), &stream(1, &other), &[]),
            Ok(false)
        );
    }

    #[test]
    fn scalars_and_types() {
        let mut x = XRefTable::new(Config::default());
        let cases = [
            (Object::name("A"), Object::name("A"), true),
            (Object::name("A"), Object::name("B"), false),
            (Object::string("A"), Object::string("A"), true),
            (Object::string("A"), Object::Hex(b"41".to_vec()), false),
            (
                Object::Hex(b"41".to_vec()),
                Object::Hex(b"41".to_vec()),
                true,
            ),
            (Object::Integer(1), Object::Integer(1), true),
            (Object::Integer(1), Object::Float(1.0), false),
            (Object::Float(1.5), Object::Float(1.5), true),
            (Object::Float(f64::NAN), Object::Float(f64::NAN), false),
            (Object::Bool(true), Object::Bool(true), true),
            (Object::Bool(true), Object::Bool(false), false),
            (Object::Null, Object::Null, true),
            (Object::Null, Object::Integer(0), false),
            (Object::Integer(0), Object::Null, false),
        ];
        for (a, b, want) in cases {
            assert_eq!(eq(&mut x, &a, &b, &[]), Ok(want), "{a:?} {b:?}");
        }
    }

    #[test]
    fn a_reference_equals_its_target() {
        let mut x = XRefTable::new(Config::default());
        x.table.insert(5, Entry::gen0(Object::Integer(9)));
        x.table.insert(6, Entry::gen0(Object::Integer(9)));
        assert_eq!(
            eq(&mut x, &Object::Ref(r(5)), &Object::Integer(9), &[]),
            Ok(true)
        );
        assert_eq!(
            eq(&mut x, &Object::Ref(r(5)), &Object::Ref(r(6)), &[]),
            Ok(true)
        );
        assert_eq!(
            eq(&mut x, &Object::Ref(r(5)), &Object::Ref(r(5)), &[]),
            Ok(true)
        );
        // An undefined reference is null.
        assert_eq!(
            eq(&mut x, &Object::Ref(r(77)), &Object::Null, &[]),
            Ok(true)
        );
        assert_eq!(
            eq(&mut x, &Object::Ref(r(77)), &Object::Integer(9), &[]),
            Ok(false)
        );
    }

    #[test]
    fn arrays_and_dicts() {
        let mut x = XRefTable::new(Config::default());
        let arr = |v: &[i64]| Object::Array(v.iter().map(|&n| Object::Integer(n)).collect());
        assert_eq!(eq(&mut x, &arr(&[1, 2]), &arr(&[1, 2]), &[]), Ok(true));
        assert_eq!(eq(&mut x, &arr(&[1, 2]), &arr(&[1, 3]), &[]), Ok(false));
        assert_eq!(eq(&mut x, &arr(&[1, 2]), &arr(&[1]), &[]), Ok(false));
        let d = |a: i64, b: i64| {
            Object::Dict(dict(&[
                ("A", Object::Integer(a)),
                ("B", Object::Integer(b)),
            ]))
        };
        assert_eq!(eq(&mut x, &d(1, 2), &d(1, 2), &[]), Ok(true));
        assert_eq!(eq(&mut x, &d(1, 2), &d(1, 3), &[]), Ok(false));
        let other_key = Object::Dict(dict(&[
            ("A", Object::Integer(1)),
            ("C", Object::Integer(2)),
        ]));
        assert_eq!(eq(&mut x, &d(1, 2), &other_key, &[]), Ok(false));
        assert_eq!(eq(&mut x, &d(1, 2), &arr(&[1, 2]), &[]), Ok(false));
    }

    #[test]
    fn font_dicts_ignore_the_subset_prefix() {
        let mut x = XRefTable::new(Config::default());
        let font = |base: &str, ty: &str| {
            Object::Dict(dict(&[
                ("Type", Object::name(ty)),
                ("BaseFont", Object::name(base)),
            ]))
        };
        assert_eq!(
            eq(
                &mut x,
                &font("ABCDEF+Arial", "Font"),
                &font("XYZABC+Arial", "Font"),
                &[]
            ),
            Ok(true)
        );
        assert_eq!(
            eq(
                &mut x,
                &font("ABCDEF+Arial", "Font"),
                &font("XYZABC+Times", "Font"),
                &[]
            ),
            Ok(false)
        );
        // Other dictionaries compare the names as they are.
        assert_eq!(
            eq(
                &mut x,
                &font("ABCDEF+Arial", "Other"),
                &font("XYZABC+Arial", "Other"),
                &[]
            ),
            Ok(false)
        );
        // A name that is no name is an error in a font dictionary.
        let bad = Object::Dict(dict(&[
            ("Type", Object::name("Font")),
            ("BaseFont", Object::Integer(1)),
        ]));
        let err = eq(&mut x, &bad, &bad.clone(), &[]).unwrap_err();
        assert!(err.to_string().contains("dict entry BaseFont"), "{err}");
        assert!(err.to_string().contains("type cast problem"), "{err}");
        // `Type` that is no name.
        let bad_type = Object::Dict(dict(&[("Type", Object::Integer(1))]));
        let err = eq(&mut x, &bad_type, &bad_type.clone(), &[]).unwrap_err();
        assert!(err.to_string().contains("first dict Type"), "{err}");
    }

    // Port of TestEqualObjectsSelectsLowestDictionaryKey: both entries fail, and Alpha is the
    // one that is reported. An entry that holds an object stream is no plain object.
    #[test]
    fn the_error_of_the_lowest_dictionary_key_is_reported() {
        let mut x = XRefTable::new(Config::default());
        for n in [3, 4, 9, 10] {
            x.table.insert(
                n,
                Entry {
                    generation: Some(0),
                    object: Some(EntryObject::ObjStm(Box::default())),
                    ..Entry::default()
                },
            );
        }
        let d1 = Object::Dict(dict(&[
            ("Zulu", Object::Ref(r(3))),
            ("Alpha", Object::Ref(r(9))),
        ]));
        let d2 = Object::Dict(dict(&[
            ("Zulu", Object::Ref(r(4))),
            ("Alpha", Object::Ref(r(10))),
        ]));
        for _ in 0..25 {
            let err = eq(&mut x, &d1, &d2, &[]).unwrap_err();
            assert!(err.to_string().contains("dict entry Alpha"), "{err}");
        }
    }

    // Port of TestGHSA_8pv5_h3hv_xhc9_SharedGraphComparisonCompletes.
    #[test]
    fn shared_graph_comparison_completes() {
        let (mut x, a, b) = shared_chains(32, false);
        assert_eq!(eq(&mut x, &a, &b, &[]), Ok(true));
    }

    // Port of TestGHSA_8pv5_h3hv_xhc9_SharedGraphMismatchIsNotMemoizedAsEqual.
    #[test]
    fn shared_graph_mismatch_is_not_memoized_as_equal() {
        let (mut x, a, b) = shared_chains(16, true);
        assert_eq!(eq(&mut x, &a, &b, &[]), Ok(false));
    }

    // Port of TestGHSA_8pv5_h3hv_xhc9_SeededPairIsRecognized.
    #[test]
    fn seeded_pair_is_recognized() {
        let mut x = XRefTable::new(Config::default());
        x.table.insert(1, Entry::gen0(Object::Integer(1)));
        x.table.insert(2, Entry::gen0(Object::Integer(2)));
        let (a, b) = (Object::Ref(r(1)), Object::Ref(r(2)));
        assert_eq!(eq(&mut x, &a, &b, &[]), Ok(false));
        // A seeded pair represents an active ancestor comparison, so meeting it closes a cycle.
        assert_eq!(eq(&mut x, &a, &b, &[2, 1]), Ok(true));
        // A trailing entry without a partner is ignored.
        assert_eq!(eq(&mut x, &a, &b, &[7]), Ok(false));
    }

    // Port of TestGHSA_8pv5_h3hv_xhc9_EqualCyclesStayEqual.
    #[test]
    fn equal_cycles_stay_equal() {
        let mut x = XRefTable::new(Config::default());
        let size = 5;
        for i in 0..size {
            x.table.insert(
                100 + i,
                Entry::gen0(Object::Array(vec![Object::Ref(r(100 + (i + 1) % size))])),
            );
            x.table.insert(
                200 + i,
                Entry::gen0(Object::Array(vec![Object::Ref(r(200 + (i + 1) % size))])),
            );
        }
        assert_eq!(
            eq(&mut x, &Object::Ref(r(100)), &Object::Ref(r(200)), &[]),
            Ok(true)
        );
    }

    // Port of TestGHSA_8pv5_h3hv_xhc9_ComparisonWorkIsBounded.
    #[test]
    fn comparison_work_is_bounded() {
        let mut x = XRefTable::new(Config::default());
        let (x_size, y_size) = (4, 5);
        let (x_base, y_base) = (100, 100 + x_size);
        for i in 0..x_size {
            let this = Object::Ref(r(x_base + i));
            let next = Object::Ref(r(x_base + (i + 1) % x_size));
            x.table
                .insert(x_base + i, Entry::gen0(Object::Array(vec![next, this])));
        }
        for i in 0..y_size {
            let this = Object::Ref(r(y_base + i));
            let next = Object::Ref(r(y_base + (i + 1) % y_size));
            x.table
                .insert(y_base + i, Entry::gen0(Object::Array(vec![this, next])));
        }
        // Beyond the work limit the objects are treated as not equal, without an error.
        assert_eq!(
            eq(
                &mut x,
                &Object::Ref(r(x_base)),
                &Object::Ref(r(y_base)),
                &[]
            ),
            Ok(false)
        );
    }

    // Port of TestGHSA_8pv5_h3hv_xhc9_ComparisonDepthIsBounded.
    #[test]
    fn comparison_depth_is_bounded() {
        let mut x = XRefTable::new(Config {
            limits: Limits {
                max_recursion_depth: 2,
                ..Limits::default()
            },
            ..Config::default()
        });
        let (mut a, mut b) = (Object::Integer(1), Object::Integer(1));
        for _ in 0..3 {
            a = Object::Array(vec![a]);
            b = Object::Array(vec![b]);
        }
        assert_eq!(eq(&mut x, &a, &b, &[]), Ok(false));
        // Two levels are within the limit.
        let two = |o: Object| Object::Array(vec![Object::Array(vec![o])]);
        assert_eq!(
            eq(
                &mut x,
                &two(Object::Integer(1)),
                &two(Object::Integer(1)),
                &[]
            ),
            Ok(true)
        );
    }

    #[test]
    fn dereferencing_errors_are_preserved() {
        let mut x = XRefTable::new(Config::default());
        x.table.insert(
            1,
            Entry {
                generation: Some(0),
                object: Some(EntryObject::ObjStm(Box::default())),
                ..Entry::default()
            },
        );
        x.table.insert(2, Entry::gen0(Object::Integer(1)));
        let err = eq(&mut x, &Object::Ref(r(1)), &Object::Ref(r(2)), &[]).unwrap_err();
        assert!(matches!(err.root(), Error::Corrupt(_)), "{err}");
    }

    #[test]
    fn a_target_that_is_a_reference_is_not_handled() {
        let mut x = XRefTable::new(Config::default());
        x.table.insert(1, Entry::gen0(Object::Ref(r(3))));
        x.table.insert(2, Entry::gen0(Object::Ref(r(3))));
        let err = eq(&mut x, &Object::Ref(r(1)), &Object::Ref(r(2)), &[]).unwrap_err();
        assert_eq!(
            err,
            Error::Corrupt("unhandled compare for type types.IndirectRef".to_string())
        );
    }

    #[test]
    fn honours_cancellation() {
        let mut x = XRefTable::new(Config::default());
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            equal_objects(&mut x, &Object::Null, &Object::Null, &[], &cancelled),
            Err(Error::Cancelled)
        );
        assert_eq!(
            equal_objects(&mut x, &Object::Null, &Object::Null, &[1, 2], &cancelled),
            Err(Error::Cancelled)
        );
    }
}
