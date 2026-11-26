//! `objectStack` (Go: pkg/pdfcpu/object_frame.go) and the addresses of the dictionaries and
//! arrays it walks.
//!
//! Go's dictionaries and arrays are shared by reference, so the optimizer changes them through
//! aliases. Here they are owned by the table entries, so a container is named by a [`Loc`]: the
//! object number of the entry that holds it (0 is the catalog) and the way from there, through
//! direct dictionaries and arrays. [`container`] resolves a [`Loc`] to the live container.
//!
//! Differences from Go:
//!
//! * Only what `fixReferencesToFreeObjects` reaches is ported: `pushDict` without its skip
//!   function, `pushArray`, `setOwner`, `owner`, `next`, `replace`, the entry label and
//!   `errorPath`. `sortTop` and the generic `label` belong to the annotation deletion, which is
//!   not reached.
//! * Keys are visited in sorted order (the determinism patch of docs/design.md section 10),
//!   which the `BTreeMap` of a [`Dict`] gives, so there is no `sortTop`.
//! * `next` reports what the entry is ([`Peek`]) and does not clone it, because a direct entry
//!   can be a whole dictionary. A frame's keys and its frame are charged to the ledger.

use crate::error::Error;
use crate::ledger::{Held, Mem};
use crate::model::{EntryObject, XRefTable};
use crate::object::{Array, Dict, Object, Ref};

/// `maxErrorPathEntries`: the entries an error path names; a longer path keeps its first and
/// last entries.
const MAX_ERROR_PATH_ENTRIES: usize = 32;

/// What a frame costs, besides its keys: the frame itself and its location.
const FRAME_COST: u64 = 128;

/// One step from a container into a direct dictionary or array inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    Key(Vec<u8>),
    Index(usize),
}

/// The address of a dictionary or an array: the table entry that holds it (0 is the catalog,
/// which resolves through [`XRefTable::catalog_mut`]) and the steps to it. The entry is a
/// dictionary, a stream (its dictionary) or an array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Loc {
    pub(crate) owner: i64,
    pub(crate) path: Vec<Step>,
}

impl Loc {
    /// The dictionary, stream dictionary or array of the table entry `owner`.
    pub(crate) fn indirect(owner: i64) -> Loc {
        Loc {
            owner,
            path: Vec::new(),
        }
    }

    /// The catalog.
    pub(crate) fn root() -> Loc {
        Loc::indirect(0)
    }

    /// The direct dictionary or array stored under `key` in this dictionary.
    pub(crate) fn key(&self, key: &[u8]) -> Loc {
        let mut path = self.path.clone();
        path.push(Step::Key(key.to_vec()));
        Loc {
            owner: self.owner,
            path,
        }
    }

    /// The direct dictionary or array at `index` in this array.
    pub(crate) fn index(&self, index: usize) -> Loc {
        let mut path = self.path.clone();
        path.push(Step::Index(index));
        Loc {
            owner: self.owner,
            path,
        }
    }
}

/// A live container.
pub(crate) enum Cont<'a> {
    Dict(&'a mut Dict),
    Array(&'a mut Array),
}

/// The container at `loc`, or `None` if it is gone (its entry was freed or replaced) or is of
/// another type.
pub(crate) fn container<'a>(x: &'a mut XRefTable, loc: &Loc) -> Option<Cont<'a>> {
    let mut cur = if loc.owner == 0 {
        Cont::Dict(x.catalog_mut().ok()?)
    } else {
        match x.table.get_mut(&loc.owner)?.object.as_mut()? {
            EntryObject::Obj(Object::Dict(d)) => Cont::Dict(d),
            EntryObject::Obj(Object::Stream(sd)) => Cont::Dict(&mut sd.dict),
            EntryObject::Obj(Object::Array(a)) => Cont::Array(a),
            _ => return None,
        }
    };
    for step in &loc.path {
        let next = match (cur, step) {
            (Cont::Dict(d), Step::Key(k)) => d.find_mut(k)?,
            (Cont::Array(a), Step::Index(i)) => a.get_mut(*i)?,
            _ => return None,
        };
        cur = match next {
            Object::Dict(d) => Cont::Dict(d),
            Object::Array(a) => Cont::Array(a),
            _ => return None,
        };
    }
    Some(cur)
}

/// What an entry of a container is, without a copy of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Peek {
    Null,
    Ref(Ref),
    Dict,
    Array,
    Other,
}

impl Peek {
    pub(crate) fn of(o: &Object) -> Peek {
        match o {
            Object::Null => Peek::Null,
            Object::Ref(r) => Peek::Ref(*r),
            Object::Dict(_) => Peek::Dict,
            Object::Array(_) => Peek::Array,
            _ => Peek::Other,
        }
    }
}

/// `objectFrame`: iterates the entries of one dictionary or array. A dictionary frame iterates
/// the keys stored at `key_start..end` in [`ObjectStack::keys`]; an array frame iterates
/// `0..end`.
struct Frame {
    loc: Loc,
    is_dict: bool,
    key_start: usize,
    /// The next entry to return.
    todo: usize,
    end: usize,
    /// The entry last returned by `next`.
    pos: usize,
    /// The number of the indirect object this frame iterates, 0 for a direct object.
    owner: i64,
    charge: u64,
}

/// `objectStack`: the frames of an explicit-stack object graph traversal and the pending keys
/// of their dictionaries. Entries of descendants are visited before the following entry of a
/// frame.
pub(crate) struct ObjectStack {
    frames: Vec<Frame>,
    keys: Vec<Vec<u8>>,
    mem: Mem,
    held: Option<Held>,
}

impl ObjectStack {
    pub(crate) fn new(mem: Mem) -> ObjectStack {
        ObjectStack {
            frames: Vec::new(),
            keys: Vec::new(),
            mem,
            held: None,
        }
    }

    pub(crate) fn empty(&self) -> bool {
        self.frames.is_empty()
    }

    fn charge(&mut self, n: u64) -> Result<(), Error> {
        match self.held.as_mut() {
            Some(h) => h.grow_by(n),
            None => {
                self.held = Some(self.mem.charge(n)?);
                Ok(())
            }
        }
    }

    fn release(&mut self, n: u64) {
        if let Some(h) = self.held.as_mut() {
            let left = h.bytes().saturating_sub(n);
            h.shrink_to(left);
        }
    }

    /// `pushDict` and `pushArray` followed by `setOwner`: schedules the entries of the
    /// container at `loc`. `owner` is the number of its indirect object, 0 for a direct one.
    /// Nothing is scheduled for a container that is gone.
    ///
    /// # Errors
    /// [`Error::Alloc`] or [`Error::OutOfMemory`] for a stack that is too big.
    pub(crate) fn push(&mut self, x: &mut XRefTable, loc: Loc, owner: i64) -> Result<(), Error> {
        let (keys, end, is_dict) = match container(x, &loc) {
            Some(Cont::Dict(d)) => {
                let mut keys = Vec::new();
                keys.try_reserve_exact(d.len())?;
                keys.extend(d.0.keys().cloned());
                let n = keys.len();
                (keys, n, true)
            }
            Some(Cont::Array(a)) => (Vec::new(), a.len(), false),
            None => return Ok(()),
        };
        let key_bytes: u64 = keys
            .iter()
            .map(|k| (k.len() as u64).saturating_add(24))
            .sum();
        let charge = FRAME_COST.saturating_add(key_bytes);
        self.charge(charge)?;
        let key_start = self.keys.len();
        self.keys.try_reserve(keys.len())?;
        self.keys.extend(keys);
        self.frames.try_reserve(1)?;
        self.frames.push(Frame {
            loc,
            is_dict,
            key_start,
            todo: if is_dict { key_start } else { 0 },
            end: if is_dict { self.keys.len() } else { end },
            pos: 0,
            owner,
            charge,
        });
        Ok(())
    }

    /// `next`: the index of the top frame and the next entry of it. If the top frame is done,
    /// it is popped and the result is `None`. An entry that is no longer in its container is
    /// null.
    pub(crate) fn next(&mut self, x: &mut XRefTable) -> Option<(usize, Peek)> {
        let i = self.frames.len().checked_sub(1)?;
        let f = self.frames.get_mut(i)?;
        if f.todo >= f.end {
            let key_start = f.key_start;
            let charge = f.charge;
            self.frames.pop();
            self.keys.truncate(key_start);
            self.release(charge);
            return None;
        }
        f.pos = f.todo;
        f.todo += 1;
        let peek = match container(x, &f.loc) {
            Some(Cont::Dict(d)) => self.keys.get(f.pos).and_then(|k| d.0.get(k)).map(Peek::of),
            Some(Cont::Array(a)) => a.get(f.pos).map(Peek::of),
            None => None,
        };
        Some((i, peek.unwrap_or(Peek::Null)))
    }

    /// The location of the entry last returned by `next` for frame `i`, which is a direct
    /// dictionary or array if [`Peek`] said so.
    pub(crate) fn entry_loc(&self, i: usize) -> Option<Loc> {
        let f = self.frames.get(i)?;
        Some(if f.is_dict {
            f.loc.key(self.keys.get(f.pos)?)
        } else {
            f.loc.index(f.pos)
        })
    }

    /// `replace`: replaces the entry last returned by `next` for frame `i`.
    pub(crate) fn replace(&mut self, x: &mut XRefTable, i: usize, o: Object) {
        let Some(f) = self.frames.get(i) else {
            return;
        };
        match container(x, &f.loc) {
            Some(Cont::Dict(d)) => {
                if let Some(k) = self.keys.get(f.pos) {
                    d.0.insert(k.clone(), o);
                }
            }
            Some(Cont::Array(a)) => {
                if let Some(slot) = a.get_mut(f.pos) {
                    *slot = o;
                }
            }
            None => {}
        }
    }

    /// `freeRefFix.label`: names the entry last returned for frame `j`, preceded by the
    /// indirect object the frame iterates.
    pub(crate) fn label(&self, j: usize) -> String {
        let Some(f) = self.frames.get(j) else {
            return String::new();
        };
        let entry = if f.is_dict {
            let key = self.keys.get(f.pos).map_or(&[][..], Vec::as_slice);
            String::from_utf8_lossy(key).into_owned()
        } else {
            format!("[{}]", f.pos)
        };
        if f.owner > 0 {
            format!("obj#{}: {entry}", f.owner)
        } else {
            entry
        }
    }
}

/// `errorPath`: joins `label(0)` .. `label(n-1)`, outermost first, with ": ". It keeps only the
/// first and last entries of a long path, so a failure at the end of a long chain costs linear
/// time and yields a bounded message.
pub(crate) fn error_path(n: usize, label: impl Fn(usize) -> String) -> String {
    fn write(b: &mut String, s: &str) {
        if !b.is_empty() {
            b.push_str(": ");
        }
        b.push_str(s);
    }
    let mut b = String::new();
    if n <= MAX_ERROR_PATH_ENTRIES {
        for j in 0..n {
            write(&mut b, &label(j));
        }
        return b;
    }
    let half = MAX_ERROR_PATH_ENTRIES / 2;
    for j in 0..half {
        write(&mut b, &label(j));
    }
    b.push_str(&format!(
        ": ... {} more entries ...",
        n - MAX_ERROR_PATH_ENTRIES
    ));
    for j in n - half..n {
        write(&mut b, &label(j));
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::Entry;

    fn r(n: i64) -> Object {
        Object::Ref(Ref {
            num: n,
            generation: 0,
        })
    }

    fn table() -> XRefTable {
        XRefTable::new(Config::default())
    }

    fn dict(entries: Vec<(&str, Object)>) -> Dict {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.set(k.as_bytes(), v);
        }
        d
    }

    #[test]
    fn walks_nested_containers_depth_first_in_key_order() {
        let mut x = table();
        let inner = Object::Array(vec![r(7), Object::Integer(1)]);
        x.table.insert(
            1,
            Entry::gen0(Object::Dict(dict(vec![
                ("B", r(5)),
                ("A", Object::Dict(dict(vec![("X", inner)]))),
                ("C", Object::Null),
            ]))),
        );
        let mut s = ObjectStack::new(x.mem().clone());
        s.push(&mut x, Loc::indirect(1), 1).unwrap();

        let mut seen = Vec::new();
        while !s.empty() {
            let Some((i, p)) = s.next(&mut x) else {
                continue;
            };
            seen.push((s.label(i), p));
            if matches!(p, Peek::Dict | Peek::Array) {
                let loc = s.entry_loc(i).unwrap();
                s.push(&mut x, loc, 0).unwrap();
            }
        }
        let labels: Vec<&str> = seen.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            ["obj#1: A", "X", "[0]", "[1]", "obj#1: B", "obj#1: C"]
        );
        assert_eq!(seen[0].1, Peek::Dict);
        assert_eq!(
            seen[2].1,
            Peek::Ref(Ref {
                num: 7,
                generation: 0
            })
        );
        assert_eq!(seen[5].1, Peek::Null);
        // The stack gave back everything it was charged.
        assert_eq!(x.mem().used(), 0);
    }

    #[test]
    fn replace_changes_the_entry_in_place() {
        let mut x = table();
        x.table.insert(
            1,
            Entry::gen0(Object::Dict(dict(vec![(
                "A",
                Object::Array(vec![Object::Integer(0), r(9)]),
            )]))),
        );
        let mut s = ObjectStack::new(x.mem().clone());
        s.push(&mut x, Loc::indirect(1), 1).unwrap();
        let (i, _) = s.next(&mut x).unwrap();
        s.push(&mut x, s.entry_loc(i).unwrap(), 0).unwrap();
        s.next(&mut x).unwrap();
        let (j, p) = s.next(&mut x).unwrap();
        assert!(matches!(p, Peek::Ref(_)));
        s.replace(&mut x, j, r(3));
        let Some(EntryObject::Obj(Object::Dict(d))) = &x.table[&1].object else {
            panic!("not a dict");
        };
        let Some(Object::Array(a)) = d.find(b"A") else {
            panic!("not an array");
        };
        assert!(matches!(
            &a[..],
            [Object::Integer(0), Object::Ref(Ref { num: 3, .. })]
        ));
    }

    #[test]
    fn a_stream_is_walked_through_its_dictionary_and_a_gone_container_is_skipped() {
        let mut x = table();
        let sd =
            crate::object::StreamDict::new(dict(vec![("K", r(2))]), 0, Some(0), None, Vec::new());
        x.table.insert(4, Entry::gen0(Object::Stream(Box::new(sd))));
        let mut s = ObjectStack::new(x.mem().clone());
        s.push(&mut x, Loc::indirect(4), 4).unwrap();
        assert!(s.next(&mut x).is_some());
        assert!(s.next(&mut x).is_none());
        assert!(s.empty());

        // No such entry: nothing is scheduled.
        s.push(&mut x, Loc::indirect(99), 99).unwrap();
        assert!(s.empty());
        assert!(container(&mut x, &Loc::indirect(4).key(b"K")).is_none());
    }

    #[test]
    fn error_path_keeps_the_ends_of_a_long_path() {
        let label = |j: usize| format!("e{j}");
        assert_eq!(error_path(0, label), "");
        assert_eq!(error_path(3, label), "e0: e1: e2");
        let long = error_path(40, label);
        assert!(long.starts_with("e0: e1: "));
        assert!(long.contains("e15: ... 8 more entries ...: e24"));
        assert!(long.ends_with("e39"));
        assert!(!long.contains("e16:"));
        assert_eq!(long.matches(": e").count(), 31);
    }
}
