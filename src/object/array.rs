//! `Array` (Go: types/array.go). Go's `Array` is a slice of `Object`; a null element is
//! [`Object::Null`].

use super::Object;
use super::string::hex_encode;

/// A PDF array.
pub type Array = Vec<Object>;

/// `NewStringLiteralArray`: string literals; each item is an escaped body.
pub fn new_string_literal_array<I>(items: I) -> Array
where
    I: IntoIterator,
    I::Item: AsRef<[u8]>,
{
    items
        .into_iter()
        .map(|s| Object::String(s.as_ref().to_vec()))
        .collect()
}

/// `NewHexLiteralArray`: hex strings of the bytes of each item.
pub fn new_hex_literal_array<I>(items: I) -> Array
where
    I: IntoIterator,
    I::Item: AsRef<[u8]>,
{
    items
        .into_iter()
        .map(|s| Object::Hex(hex_encode(s.as_ref())))
        .collect()
}

/// `NewNameArray`.
pub fn new_name_array<I>(items: I) -> Array
where
    I: IntoIterator,
    I::Item: AsRef<[u8]>,
{
    items
        .into_iter()
        .map(|s| Object::Name(s.as_ref().to_vec()))
        .collect()
}

/// `NewNumberArray`: floats.
pub fn new_number_array(items: &[f64]) -> Array {
    items.iter().map(|&f| Object::Float(f)).collect()
}

/// `NewIntegerArray`.
pub fn new_integer_array(items: &[i64]) -> Array {
    items.iter().map(|&i| Object::Integer(i)).collect()
}

/// `RemoveNulls`: `a` without its null elements.
pub fn remove_nulls(a: Array) -> Array {
    a.into_iter().filter(|o| !o.is_null()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_and_remove_nulls() {
        let a = new_name_array(["A", "B"]);
        assert!(matches!(&a[..], [Object::Name(x), Object::Name(y)] if x == b"A" && y == b"B"));
        let h = new_hex_literal_array(["AB"]);
        assert!(matches!(&h[..], [Object::Hex(x)] if x == b"4142"));
        let s = new_string_literal_array(["a(b"]);
        assert!(matches!(&s[..], [Object::String(x)] if x == b"a(b"));
        let n = new_number_array(&[1.0, 2.5]);
        assert!(matches!(&n[..], [Object::Float(x), Object::Float(y)] if *x == 1.0 && *y == 2.5));
        let i = new_integer_array(&[1, 2]);
        assert!(matches!(&i[..], [Object::Integer(1), Object::Integer(2)]));

        let with_nulls = vec![Object::Null, Object::Integer(1), Object::Null];
        let kept = remove_nulls(with_nulls);
        assert!(matches!(&kept[..], [Object::Integer(1)]));
        assert!(remove_nulls(Vec::new()).is_empty());
    }
}
