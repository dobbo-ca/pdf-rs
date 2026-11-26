//! `subtle.ConstantTimeCompare`.

/// Whether `a` and `b` are equal, without stopping at the first difference. Different lengths
/// are unequal at once, as in Go.
pub fn eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::eq;

    // Port of TestPasswordHashEqual.
    #[test]
    fn password_hash_equal() {
        assert!(eq(b"digest", b"digest"));
        assert!(!eq(b"digest", b"digesu"));
        assert!(!eq(b"digest", b"digest-longer"));
    }
}
