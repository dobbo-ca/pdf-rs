//! `Dict` (Go: types/dict.go). Go's `Dict` is a map from string to `Object`; this one is a
//! `BTreeMap` keyed by the decoded name bytes, so iteration is in the order of Go's
//! `sort.Strings` (docs/design.md section 4). Go's `nil` object is [`Object::Null`].

use super::string::{decode_name, hex_decode, unescape};
use super::text::string_or_hex_literal;
use super::{Array, Object, Ref};
use crate::error::Error;
use std::collections::BTreeMap;

/// A PDF dictionary. The field is public for iteration and for the callers that need Go's plain
/// map assignment; the methods keep Go's semantics (`insert` does not overwrite, `find` also
/// matches a key that is stored in its encoded form).
#[derive(Debug, Clone, Default)]
pub struct Dict(pub BTreeMap<Vec<u8>, Object>);

impl Dict {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The entries in key order.
    pub fn iter(&self) -> std::collections::btree_map::Iter<'_, Vec<u8>, Object> {
        self.0.iter()
    }

    /// The entries in key order, values mutable.
    pub fn iter_mut(&mut self) -> std::collections::btree_map::IterMut<'_, Vec<u8>, Object> {
        self.0.iter_mut()
    }

    /// Go's `d[key] = value`: sets the entry, replacing an existing one.
    pub fn set(&mut self, key: &[u8], value: Object) {
        self.0.insert(key.to_vec(), value);
    }

    /// The stored key `Find` would return the value of: the key itself, else the first key (in
    /// key order; Go's map order is random) whose decoded form is `key`. Go gives up at the
    /// first key that does not decode.
    fn find_key(&self, key: &[u8]) -> Option<&Vec<u8>> {
        if let Some((k, _)) = self.0.get_key_value(key) {
            return Some(k);
        }
        for k in self.0.keys() {
            match decode_name(k) {
                Err(_) => return None,
                Ok(d) if *d == *key => return Some(k),
                Ok(_) => {}
            }
        }
        None
    }

    /// `Find`: the object for `key`. `Some(Object::Null)` is an entry whose value is null.
    pub fn find(&self, key: &[u8]) -> Option<&Object> {
        let k = self.find_key(key)?;
        self.0.get(k)
    }

    /// `Find` for in-place changes (Go returns the stored value, so writes through it persist).
    pub fn find_mut(&mut self, key: &[u8]) -> Option<&mut Object> {
        let k = self.find_key(key)?.clone();
        self.0.get_mut(&k)
    }

    /// `Insert`: adds a new entry; false, and no change, if `key` is already there.
    pub fn insert(&mut self, key: &[u8], value: Object) -> bool {
        if self.find_key(key).is_some() {
            return false;
        }
        self.0.insert(key.to_vec(), value);
        true
    }

    pub fn insert_bool(&mut self, key: &[u8], value: bool) {
        self.insert(key, Object::Bool(value));
    }

    pub fn insert_int(&mut self, key: &[u8], value: i64) {
        self.insert(key, Object::Integer(value));
    }

    /// `InsertFloat` takes a `float32`.
    pub fn insert_float(&mut self, key: &[u8], value: f32) {
        self.insert(key, Object::Float(f64::from(value)));
    }

    /// `InsertString`: a string literal; `value` is the escaped body.
    pub fn insert_string(&mut self, key: &[u8], value: &[u8]) {
        self.insert(key, Object::String(value.to_vec()));
    }

    pub fn insert_name(&mut self, key: &[u8], value: &[u8]) {
        self.insert(key, Object::Name(value.to_vec()));
    }

    /// `Update`: replaces the entry; a null `value` changes nothing.
    pub fn update(&mut self, key: &[u8], value: Object) {
        if !value.is_null() {
            self.set(key, value);
        }
    }

    /// `HasEntry`: the key exists and maps to a non-null object.
    pub fn has_entry(&self, key: &[u8]) -> bool {
        self.find(key).is_some_and(|o| !o.is_null())
    }

    /// `Delete`: removes the entry stored under exactly `key` and returns its value. Like Go it
    /// also returns the value of an entry found through its encoded key, which it does not
    /// remove.
    pub fn delete(&mut self, key: &[u8]) -> Option<Object> {
        let v = self.find(key)?.clone();
        self.0.remove(key);
        Some(v)
    }

    /// `Entry`: the object for `key` (`None` when absent or null) and whether the entry was
    /// found.
    ///
    /// # Errors
    /// [`Error::Validation`] if `required` and the entry is missing or null.
    pub fn entry(
        &self,
        dict_name: &str,
        key: &[u8],
        required: bool,
    ) -> Result<(Option<&Object>, bool), Error> {
        let Some(obj) = self.find(key) else {
            if required {
                return Err(Error::Validation(format!(
                    "dict={dict_name} required entry={} missing",
                    String::from_utf8_lossy(key)
                )));
            }
            return Ok((None, false));
        };
        if obj.is_null() {
            if required {
                return Err(Error::Validation(format!(
                    "dict={dict_name} required entry={} corrupt",
                    String::from_utf8_lossy(key)
                )));
            }
            return Ok((None, true));
        }
        Ok((Some(obj), true))
    }

    /// `BooleanEntry`.
    pub fn boolean_entry(&self, key: &[u8]) -> Option<bool> {
        match self.find(key)? {
            Object::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// `StringEntry` and `StringLiteralEntry`: the escaped body of a string literal.
    pub fn string_entry(&self, key: &[u8]) -> Option<&[u8]> {
        match self.find(key)? {
            Object::String(s) => Some(s),
            _ => None,
        }
    }

    /// `NameEntry`: the decoded name.
    pub fn name_entry(&self, key: &[u8]) -> Option<&[u8]> {
        match self.find(key)? {
            Object::Name(n) => Some(n),
            _ => None,
        }
    }

    /// `IntEntry` and `Int64Entry` (Go's `int` is 64 bits).
    pub fn int_entry(&self, key: &[u8]) -> Option<i64> {
        match self.find(key)? {
            Object::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// `IndirectRefEntry`.
    pub fn indirect_ref_entry(&self, key: &[u8]) -> Option<Ref> {
        match self.find(key)? {
            Object::Ref(r) => Some(*r),
            _ => None,
        }
    }

    /// `DictEntry`: a direct dictionary (a stream does not count, as in Go).
    pub fn dict_entry(&self, key: &[u8]) -> Option<&Dict> {
        match self.find(key)? {
            Object::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// `ArrayEntry`.
    pub fn array_entry(&self, key: &[u8]) -> Option<&Array> {
        match self.find(key)? {
            Object::Array(a) => Some(a),
            _ => None,
        }
    }

    /// `HexLiteralEntry`: the digits of a hex string, as stored.
    pub fn hex_literal_entry(&self, key: &[u8]) -> Option<&[u8]> {
        match self.find(key)? {
            Object::Hex(h) => Some(h),
            _ => None,
        }
    }

    /// `StringOrHexLiteralEntry`: the text of a string or hex string entry; `Ok(None)` if the key
    /// is absent.
    ///
    /// # Errors
    /// As [`string_or_hex_literal`](super::text::string_or_hex_literal).
    pub fn string_or_hex_literal_entry(&self, key: &[u8]) -> Result<Option<String>, Error> {
        match self.find(key) {
            Some(obj) => string_or_hex_literal(obj).map(Some),
            None => Ok(None),
        }
    }

    /// `StringEntryBytes`: the bytes of a string or hex string entry; `Ok(None)` if there is
    /// neither.
    ///
    /// # Errors
    /// An invalid hex digit.
    pub fn string_entry_bytes(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        if let Some(s) = self.string_entry(key) {
            return Ok(Some(unescape(s)));
        }
        if let Some(h) = self.hex_literal_entry(key) {
            return hex_decode(h).map(Some);
        }
        Ok(None)
    }

    /// `Length`: `(Some(n), None)` for a direct `/Length`, `(None, Some(object number))` for an
    /// indirect one, `(None, None)` otherwise.
    pub fn length(&self) -> (Option<i64>, Option<i64>) {
        if let Some(n) = self.int_entry(b"Length") {
            return (Some(n), None);
        }
        match self.indirect_ref_entry(b"Length") {
            Some(r) => (None, Some(r.num)),
            None => (None, None),
        }
    }

    /// `Type`.
    pub fn type_(&self) -> Option<&[u8]> {
        self.name_entry(b"Type")
    }

    /// `Subtype`.
    pub fn subtype(&self) -> Option<&[u8]> {
        self.name_entry(b"Subtype")
    }

    /// `Size`.
    pub fn size(&self) -> Option<i64> {
        self.int_entry(b"Size")
    }

    /// `IsPage`.
    pub fn is_page(&self) -> bool {
        self.type_() == Some(b"Page".as_slice())
    }

    /// `IsObjStm`.
    pub fn is_obj_stm(&self) -> bool {
        self.type_() == Some(b"ObjStm".as_slice())
    }

    /// `W`.
    pub fn w(&self) -> Option<&Array> {
        self.array_entry(b"W")
    }

    /// `Prev`.
    pub fn prev(&self) -> Option<i64> {
        self.int_entry(b"Prev")
    }

    /// `Index`.
    pub fn index(&self) -> Option<&Array> {
        self.array_entry(b"Index")
    }

    /// `N`.
    pub fn n(&self) -> Option<i64> {
        self.int_entry(b"N")
    }

    /// `First`.
    pub fn first(&self) -> Option<i64> {
        self.int_entry(b"First")
    }

    /// `IsLinearizationParmDict`: a direct number entry with the value 1 for `/Linearized`.
    pub fn is_linearization_parm_dict(&self) -> bool {
        match self.find(b"Linearized") {
            Some(Object::Integer(i)) => *i == 1,
            Some(Object::Float(f)) => *f == 1.0,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(entries: Vec<(&str, Object)>) -> Dict {
        Dict(
            entries
                .into_iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v))
                .collect(),
        )
    }

    #[test]
    fn has_entry_null_missing_and_encoded_key() {
        let d = dict(vec![
            ("A", Object::Integer(1)),
            ("B", Object::Null),
            ("C#28#29", Object::name("encoded")),
        ]);
        assert!(d.has_entry(b"A"));
        // A null entry is found but does not count.
        assert!(d.find(b"B").is_some());
        assert!(!d.has_entry(b"B"));
        assert!(!d.has_entry(b"D"));
        // The key is stored encoded; the decoded name finds it.
        assert!(d.has_entry(b"C()"));
    }

    #[test]
    fn find_gives_up_at_a_key_that_does_not_decode() {
        let d = dict(vec![
            ("A#", Object::Integer(1)),
            ("B", Object::Integer(2)),
            ("C#41", Object::Integer(3)),
        ]);
        // Direct hit.
        assert_eq!(d.int_entry(b"B"), Some(2));
        // "A#" does not decode and sorts before "C#41", so the scan stops there.
        assert_eq!(d.int_entry(b"CA"), None);
    }

    #[test]
    fn insert_keeps_an_existing_entry() {
        let mut d = Dict::default();
        assert!(d.insert(b"K", Object::Integer(1)));
        assert!(!d.insert(b"K", Object::Integer(2)));
        assert_eq!(d.int_entry(b"K"), Some(1));
        d.update(b"K", Object::Integer(3));
        assert_eq!(d.int_entry(b"K"), Some(3));
        d.update(b"K", Object::Null);
        assert_eq!(d.int_entry(b"K"), Some(3));
        d.set(b"K", Object::Null);
        assert!(!d.has_entry(b"K"));
        d.insert_int(b"I", 7);
        d.insert_bool(b"T", true);
        d.insert_name(b"N", b"Nm");
        d.insert_string(b"S", b"s\\(");
        d.insert_float(b"F", 1.5);
        assert_eq!(d.int_entry(b"I"), Some(7));
        assert_eq!(d.boolean_entry(b"T"), Some(true));
        assert_eq!(d.name_entry(b"N"), Some(b"Nm".as_slice()));
        assert_eq!(d.string_entry(b"S"), Some(b"s\\(".as_slice()));
        assert!(matches!(d.find(b"F"), Some(Object::Float(f)) if *f == 1.5));
        assert_eq!(d.len(), 6);
    }

    #[test]
    fn delete_returns_the_value() {
        let mut d = dict(vec![
            ("A", Object::Integer(1)),
            ("B#41", Object::Integer(2)),
        ]);
        assert!(matches!(d.delete(b"A"), Some(Object::Integer(1))));
        assert!(d.find(b"A").is_none());
        assert!(d.delete(b"A").is_none());
        // Found through its encoded key: the value comes back, the entry stays.
        assert!(matches!(d.delete(b"BA"), Some(Object::Integer(2))));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn typed_entries_check_the_type() {
        let d = dict(vec![
            ("Int", Object::Integer(5)),
            (
                "Ref",
                Object::Ref(Ref {
                    num: 3,
                    generation: 1,
                }),
            ),
            ("Dict", Object::Dict(Dict::default())),
            ("Arr", Object::Array(vec![Object::Null])),
            ("Hex", Object::Hex(b"4142".to_vec())),
            ("Str", Object::String(b"A\\101".to_vec())),
            ("Type", Object::name("Page")),
            ("Linearized", Object::Float(1.0)),
        ]);
        assert_eq!(d.int_entry(b"Int"), Some(5));
        assert_eq!(d.int_entry(b"Str"), None);
        assert_eq!(
            d.indirect_ref_entry(b"Ref"),
            Some(Ref {
                num: 3,
                generation: 1
            })
        );
        assert!(d.dict_entry(b"Dict").is_some());
        assert!(d.dict_entry(b"Arr").is_none());
        assert_eq!(d.array_entry(b"Arr").map(Vec::len), Some(1));
        assert_eq!(d.hex_literal_entry(b"Hex"), Some(b"4142".as_slice()));
        assert_eq!(
            d.string_entry_bytes(b"Str").ok(),
            Some(Some(b"AA".to_vec()))
        );
        assert_eq!(
            d.string_entry_bytes(b"Hex").ok(),
            Some(Some(b"AB".to_vec()))
        );
        assert_eq!(d.string_entry_bytes(b"Int").ok(), Some(None));
        assert_eq!(
            d.string_or_hex_literal_entry(b"Hex").ok(),
            Some(Some("AB".to_string()))
        );
        assert_eq!(d.string_or_hex_literal_entry(b"Nope").ok(), Some(None));
        assert!(d.string_or_hex_literal_entry(b"Int").is_err());
        assert!(d.is_page());
        assert!(!d.is_obj_stm());
        assert!(d.is_linearization_parm_dict());
    }

    #[test]
    fn length_direct_or_indirect() {
        let mut d = Dict::default();
        assert_eq!(d.length(), (None, None));
        d.set(
            b"Length",
            Object::Ref(Ref {
                num: 9,
                generation: 0,
            }),
        );
        assert_eq!(d.length(), (None, Some(9)));
        d.set(b"Length", Object::Integer(42));
        assert_eq!(d.length(), (Some(42), None));
    }

    #[test]
    fn entry_required_and_optional() {
        let d = dict(vec![("A", Object::Integer(1)), ("N", Object::Null)]);
        assert!(matches!(d.entry("D", b"A", true), Ok((Some(_), true))));
        assert!(matches!(d.entry("D", b"X", false), Ok((None, false))));
        assert!(matches!(d.entry("D", b"N", false), Ok((None, true))));
        assert_eq!(
            d.entry("D", b"X", true).err(),
            Some(Error::Validation("dict=D required entry=X missing".into()))
        );
        assert_eq!(
            d.entry("D", b"N", true).err(),
            Some(Error::Validation("dict=D required entry=N corrupt".into()))
        );
    }
}
