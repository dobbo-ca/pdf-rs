//! Internalised name trees (Go: pkg/pdfcpu/model/nameTree.go `Node` and `entry`,
//! name_tree_keys.go `nameTreeEntry` and `rawLimits`; xreftable.go:1607-1744 `BindNameTrees`,
//! :1817-1830 `NamesDict`).
//!
//! The validator builds one [`Node`] tree per name tree of the catalog (`/Dests`, `/JavaScript`,
//! ...) and keeps it in [`XRefTable::names`]; the writer syncs the trees back into the table with
//! [`XRefTable::bind_name_trees`] before it writes the catalog.
//!
//! Not ported, because nothing the port reaches edits a name tree: `Add`, `Remove`, `Process`,
//! `Value`, the split and merge code and the text index of name_tree_keys.go.
//!
//! Differences from Go:
//!
//! * Go's `Node.D` shares its map with the table object, so writes through one show in the
//!   other. Here [`Node::d`] is a copy. [`XRefTable::bind_name_trees`] updates the copy and then
//!   stores it in the `/Names` dictionary of the catalog, which is what Go's shared map gives.
//! * Go walks the tree with an explicit stack. Here it is a recursion, bounded by the configured
//!   recursion depth (the validator enforces the same bound when it builds the tree).

use super::deref::type_name;
use super::xref::{ENTRY_BYTES, XRefTable};
use crate::error::{Error, Sentinel};
use crate::object::array::new_hex_literal_array;
use crate::object::string::{hex_decode, unescape};
use crate::object::text::string_or_hex_literal;
use crate::object::{Dict, Object, dict_copy_bytes};

/// The heap of a hex string object of `n` bytes in an array: its slot and its digits.
fn hex_slot_bytes(n: usize) -> u64 {
    (size_of::<Object>() + 2 * n) as u64
}

/// One key and value of a name tree leaf (Go's `entry`).
#[derive(Debug, Clone)]
pub struct NameEntry {
    /// The key as decoded text.
    pub k: String,
    pub v: Object,
    /// The key as the PDF string bytes it was read from (7.9.6), when they are known.
    pub raw: Option<Vec<u8>>,
}

impl NameEntry {
    /// `entry.key`: the bytes of the key as the tree is ordered by them.
    pub fn key(&self) -> &[u8] {
        match &self.raw {
            Some(r) => r,
            None => self.k.as_bytes(),
        }
    }
}

/// `Node`: a node of a name tree.
#[derive(Debug, Clone, Default)]
pub struct Node {
    /// The immediate children, for an intermediate node.
    pub kids: Vec<Node>,
    /// The keys and values, for a leaf.
    pub names: Vec<NameEntry>,
    /// The `/Limits` of the node, as decoded text.
    pub kmin: String,
    pub kmax: String,
    /// The dictionary of this node; `None` is Go's nil map.
    pub d: Option<Dict>,
}

/// `nameTreeEntry`: the PDF string bytes of the key `k` that sits at `index` of the `/Names`
/// array of `d`, if that element is the string `k`.
fn raw_key(k: &str, d: Option<&Dict>, index: usize) -> Option<Vec<u8>> {
    let o = d?.array_entry(b"Names")?.get(index)?;
    let text = string_or_hex_literal(o).ok()?;
    if text != k {
        return None;
    }
    match o {
        Object::String(s) => Some(unescape(s)),
        Object::Hex(h) => hex_decode(h).ok(),
        _ => None,
    }
}

impl Node {
    /// A node for the dictionary `d`.
    pub fn new(d: Dict) -> Node {
        Node {
            d: Some(d),
            ..Node::default()
        }
    }

    /// `leaf`.
    pub fn leaf(&self) -> bool {
        self.kids.is_empty()
    }

    /// `AppendToNames`: adds an entry to a leaf node, for internalising a name tree. The PDF bytes
    /// of the key are taken from the `/Names` array of the node's dictionary.
    pub fn append_to_names(&mut self, k: &str, v: Object) {
        let raw = raw_key(k, self.d.as_ref(), self.names.len().saturating_mul(2));
        self.names.push(NameEntry {
            k: k.to_string(),
            v,
            raw,
        });
    }

    /// `rawLimits`: the first key of the first leaf and the last key of the last leaf, as PDF
    /// bytes; `kmin` and `kmax` where there is no entry.
    pub fn raw_limits(&self) -> (Vec<u8>, Vec<u8>) {
        let mut first = self;
        while let Some(k) = first.kids.first() {
            first = k;
        }
        let mut last = self;
        while let Some(k) = last.kids.last() {
            last = k;
        }
        let lo = first
            .names
            .first()
            .map_or_else(|| self.kmin.as_bytes().to_vec(), |e| e.key().to_vec());
        let hi = last
            .names
            .last()
            .map_or_else(|| self.kmax.as_bytes().to_vec(), |e| e.key().to_vec());
        (lo, hi)
    }
}

impl XRefTable {
    /// `NamesDict`, to change it: the dictionary that holds all name trees, which is the
    /// catalog's `/Names`. It is created, as a new object, if the catalog has none. `None` is the
    /// null object.
    ///
    /// # Errors
    /// The catalog errors of [`XRefTable::catalog`], [`Sentinel::ExpectedDict`] for a `/Names`
    /// that is no dictionary, and the errors of an insert.
    fn names_dict_mut(&mut self) -> Result<Option<&mut Dict>, Error> {
        let found = self.catalog()?.find(b"Names").cloned();
        match found {
            None => {
                self.held.grow_by(ENTRY_BYTES)?;
                let r = self.ind_ref_for_new_object(Object::Dict(Dict::default()))?;
                self.catalog_mut()?.set(b"Names", Object::Ref(r));
                self.dereference_dict_mut(r)
            }
            Some(Object::Ref(r)) => self.dereference_dict_mut(r),
            Some(Object::Dict(_)) => Ok(match self.catalog_mut()?.find_mut(b"Names") {
                Some(Object::Dict(d)) => Some(d),
                _ => None,
            }),
            Some(Object::Null) => Ok(None),
            Some(other) => Err(Error::Sentinel(Sentinel::ExpectedDict)
                .context(format!("expected types.Dict, got {}", type_name(&other)))),
        }
    }

    /// `bindNameTreeNodeDict`: makes sure the node has a dictionary. For a root node that has one,
    /// checks that the catalog has a `/Names` dictionary to register it in (creating it if
    /// needed). Returns whether the caller has to register the node's dictionary there when it is
    /// done with it.
    fn bind_name_tree_node_dict(&mut self, n: &mut Node, root: bool) -> Result<bool, Error> {
        if n.d.is_none() {
            n.d = Some(Dict::default());
            return Ok(false);
        }
        if root && self.names_dict_mut()?.is_none() {
            return Err(Error::Corrupt("root entry \"Names\" corrupt".into()));
        }
        Ok(root)
    }

    /// `bindNameTreeNodeFrame`: writes the limits and the leaf or kid entries of `n` into its
    /// dictionary. The kids have been bound already.
    fn bind_name_tree_node_frame(
        &mut self,
        name: &str,
        n: &mut Node,
        root: bool,
    ) -> Result<(), Error> {
        let register = self.bind_name_tree_node_dict(n, root).map_err(|e| {
            e.context(if root {
                "root dictionary"
            } else {
                "node dictionary"
            })
        })?;

        let (lo, hi) = n.raw_limits();
        let Some(dict) = n.d.as_mut() else {
            return Err(Error::Panic("nil name tree dictionary"));
        };

        // bindNameTreeNodeLimits
        if root {
            dict.delete(b"Limits");
        } else {
            // What the dictionary keeps of the node is charged here, for the table's life.
            self.held
                .grow_by(hex_slot_bytes(lo.len()) + hex_slot_bytes(hi.len()))?;
            dict.update(b"Limits", Object::Array(new_hex_literal_array([lo, hi])));
        }

        if n.kids.is_empty() {
            // bindNameTreeLeafNode
            let bytes = n
                .names
                .iter()
                .map(|e| {
                    hex_slot_bytes(e.key().len()) + size_of::<Object>() as u64 + e.v.copy_bytes()
                })
                .sum();
            self.held.grow_by(bytes)?;
            let mut a = Vec::new();
            a.try_reserve(n.names.len().saturating_mul(2))?;
            for e in &n.names {
                a.push(Object::hex_from_bytes(e.key()));
                a.push(e.v.clone());
            }
            dict.update(b"Names", Object::Array(a));
        } else {
            // bindNameTreeIntermediateNode
            let mut kids = Vec::new();
            kids.try_reserve(n.kids.len())?;
            for (i, k) in n.kids.iter().enumerate() {
                let d = k.d.as_ref().map_or(0, dict_copy_bytes);
                self.held
                    .grow_by(ENTRY_BYTES + size_of::<Object>() as u64 + d)?;
                let r = self
                    .ind_ref_for_new_object(Object::Dict(k.d.clone().unwrap_or_default()))
                    .map_err(|e| {
                        e.context(format!("child object {i}: insert"))
                            .context("intermediate node")
                    })?;
                kids.push(Object::Ref(r));
            }
            dict.update(b"Kids", Object::Array(kids));
            dict.delete(b"Names");
        }

        if register {
            // Go's shared map makes the table's `/Names` entry follow the node's dictionary.
            self.held.grow_by(n.d.as_ref().map_or(0, dict_copy_bytes))?;
            let d = n.d.clone().unwrap_or_default();
            match self.names_dict_mut()? {
                Some(nd) => nd.set(name.as_bytes(), Object::Dict(d)),
                None => return Err(Error::Corrupt("root entry \"Names\" corrupt".into())),
            }
        }
        Ok(())
    }

    /// `bindNameTreeNode`: binds the kids of `n` first, then `n`.
    fn bind_name_tree_node(
        &mut self,
        name: &str,
        n: &mut Node,
        root: bool,
        depth: u64,
    ) -> Result<(), Error> {
        self.check_recursion_depth("name tree", depth)?;
        for k in n.kids.iter_mut() {
            self.bind_name_tree_node(name, k, false, depth.saturating_add(1))?;
        }
        self.bind_name_tree_node_frame(name, n, root)
    }

    /// `BindNameTrees`: syncs up the internal name tree cache with the table. The trees are bound
    /// in the order of their names.
    ///
    /// # Errors
    /// `name tree "<name>": root dictionary: ...` (or `node dictionary`, or `intermediate node:
    /// child object <i>: insert: ...`) with the cause: the catalog errors, an insert into the
    /// free list that fails, and [`Sentinel::MaxRecursionDepth`] for a tree that is too deep.
    pub fn bind_name_trees(&mut self) -> Result<(), Error> {
        let mut names = std::mem::take(&mut self.names);
        let mut result = Ok(());
        for (name, tree) in names.iter_mut() {
            if let Err(e) = self.bind_name_tree_node(name, tree, true, 0) {
                result = Err(e.context(format!("name tree {name:?}")));
                break;
            }
        }
        self.names = names;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::xref::Entry;
    use crate::object::Ref;

    fn table() -> XRefTable {
        XRefTable::new(Config::default())
    }

    fn leaf(k: &str, v: &str) -> Node {
        Node {
            kmin: k.into(),
            kmax: k.into(),
            names: vec![NameEntry {
                k: k.into(),
                v: Object::string(v),
                raw: None,
            }],
            ..Node::default()
        }
    }

    fn must_fail(x: &mut XRefTable, want: &[&str]) {
        let err = x.bind_name_trees().unwrap_err().to_string();
        for s in want {
            assert!(err.contains(s), "expected {s:?} in {err:?}");
        }
    }

    // Port of TestBindNameTreesAddsTreeNameAndRootDictionaryContext.
    #[test]
    fn bind_adds_tree_name_and_root_dictionary_context() {
        let mut x = table();
        x.names.insert(
            "JavaScript".into(),
            Node {
                d: Some(Dict::default()),
                ..leaf("script", "value")
            },
        );
        must_fail(
            &mut x,
            &[
                "name tree \"JavaScript\"",
                "root dictionary",
                "missing root dict",
            ],
        );
    }

    // Port of TestBindNameTreesDistinguishesIntermediateAndChildInsertion.
    #[test]
    fn bind_distinguishes_intermediate_and_child_insertion() {
        let mut x = table();
        x.table.insert(0, Entry::gen0(Object::Null));
        x.names.insert(
            "JavaScript".into(),
            Node {
                kmin: "a".into(),
                kmax: "z".into(),
                kids: vec![leaf("a", "a"), leaf("z", "z")],
                ..Node::default()
            },
        );
        must_fail(
            &mut x,
            &[
                "name tree \"JavaScript\"",
                "intermediate node",
                "child object 0",
                "insert",
                "object #0 found, but not free",
            ],
        );
        // The tree is back in the table after a failure.
        assert!(x.names.contains_key("JavaScript"));
    }

    fn doc_with_catalog() -> XRefTable {
        let mut x = table();
        x.table.insert(0, Entry::free_head());
        let mut cat = Dict::default();
        cat.insert_name(b"Type", b"Catalog");
        x.table.insert(1, Entry::gen0(Object::Dict(cat)));
        x.root = Some(Ref {
            num: 1,
            generation: 0,
        });
        x.size = Some(2);
        x
    }

    #[test]
    fn bind_leaf_creates_the_names_dictionary() {
        let mut x = doc_with_catalog();
        x.names.insert(
            "JavaScript".into(),
            Node {
                d: Some(Dict::default()),
                ..leaf("b", "x")
            },
        );
        x.bind_name_trees().unwrap();

        let names_ref = match x.catalog().unwrap().find(b"Names") {
            Some(Object::Ref(r)) => *r,
            other => panic!("Names: {other:?}"),
        };
        assert_eq!(names_ref.num, 2);
        let nd = x.dereference_dict_ref(names_ref).unwrap().unwrap().clone();
        let tree = nd.dict_entry(b"JavaScript").unwrap();
        // The root has no Limits; its leaf carries the key as hex.
        assert!(tree.find(b"Limits").is_none());
        let a = tree.array_entry(b"Names").unwrap();
        assert_eq!(a.len(), 2);
        assert!(matches!(&a[0], Object::Hex(h) if h == b"62"));
        assert!(matches!(&a[1], Object::String(s) if s == b"x"));
    }

    #[test]
    fn bind_intermediate_inserts_the_kids_and_their_limits() {
        let mut x = doc_with_catalog();
        x.names.insert(
            "Dests".into(),
            Node {
                d: Some(Dict::default()),
                kmin: "a".into(),
                kmax: "z".into(),
                kids: vec![leaf("a", "1"), leaf("z", "2")],
                ..Node::default()
            },
        );
        x.bind_name_trees().unwrap();

        // Object 2 is the Names dictionary; the root's kids follow it, as in Go's post-order.
        let nd = x
            .dereference_dict_ref(Ref {
                num: 2,
                generation: 0,
            })
            .unwrap()
            .unwrap()
            .clone();
        let tree = nd.dict_entry(b"Dests").unwrap();
        let kids = tree.array_entry(b"Kids").unwrap();
        assert!(tree.find(b"Names").is_none());
        assert_eq!(kids.len(), 2);
        let first = match &kids[0] {
            Object::Ref(r) => *r,
            other => panic!("kid: {other:?}"),
        };
        assert_eq!(first.num, 3);
        let kid = x.dereference_dict_ref(first).unwrap().unwrap();
        let limits = kid.array_entry(b"Limits").unwrap();
        assert!(matches!(&limits[0], Object::Hex(h) if h == b"61"));
        assert!(matches!(&limits[1], Object::Hex(h) if h == b"61"));
    }

    #[test]
    fn raw_limits_prefer_the_pdf_bytes() {
        let mut n = Node::default();
        n.names.push(NameEntry {
            k: "a".into(),
            v: Object::Null,
            raw: Some(vec![0xFE, 0xFF, 0, b'a']),
        });
        n.kmin = "x".into();
        n.kmax = "y".into();
        let (lo, hi) = n.raw_limits();
        assert_eq!(lo, [0xFE, 0xFF, 0, b'a']);
        assert_eq!(hi, [0xFE, 0xFF, 0, b'a']);

        // No entries: the limits as text.
        let e = Node {
            kmin: "x".into(),
            kmax: "y".into(),
            ..Node::default()
        };
        assert_eq!(e.raw_limits(), (b"x".to_vec(), b"y".to_vec()));

        // An intermediate node takes the first and the last leaf.
        let p = Node {
            kids: vec![leaf("a", "1"), leaf("m", "2"), leaf("z", "3")],
            ..Node::default()
        };
        assert_eq!(p.raw_limits(), (b"a".to_vec(), b"z".to_vec()));
    }

    #[test]
    fn append_to_names_keeps_the_pdf_bytes_of_the_key() {
        let mut d = Dict::default();
        d.set(
            b"Names",
            Object::Array(vec![Object::Hex(b"4142".to_vec()), Object::Null]),
        );
        let mut n = Node::new(d);
        n.append_to_names("AB", Object::Integer(1));
        assert_eq!(n.names[0].raw.as_deref(), Some(b"AB".as_slice()));
        // The second key has no element in the array: no raw bytes.
        n.append_to_names("C", Object::Integer(2));
        assert!(n.names[1].raw.is_none());
        assert_eq!(n.names[1].key(), b"C");
    }
}
