//! The working memory of a document: every buffer that scales with the input is charged before
//! it is allocated and released when it drops, by all threads together (docs/design.md section
//! 7). It is tiff-rs's ledger made owned: a [`Held`] charge keeps an `Arc` of its [`Ledger`], so
//! it can live in a `Context` between calls.

use crate::config::Limits;
use crate::error::Error;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Bytes booked against `Limits::max_alloc`, shared by every thread that uses the document.
#[derive(Debug)]
pub struct Ledger {
    max: u64,
    used: AtomicU64,
    peak: AtomicU64,
}

impl Ledger {
    fn new(max: u64) -> Self {
        Self {
            max,
            used: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        }
    }

    /// Books `n` bytes, or says what was left.
    fn reserve(&self, n: u64) -> Result<(), Error> {
        let r = self
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |u| {
                u.checked_add(n).filter(|&t| t <= self.max)
            });
        match r {
            Ok(u) => {
                self.peak.fetch_max(u.saturating_add(n), Ordering::Relaxed);
                Ok(())
            }
            Err(u) => Err(Error::Alloc {
                got: n,
                max: self.max.saturating_sub(u),
            }),
        }
    }

    fn release(&self, n: u64) {
        self.used.fetch_sub(n, Ordering::AcqRel);
    }
}

/// The cloneable handle to a document's ledger: `ctx.xref.mem()` for a document, or
/// [`Mem::new`] on its own for a call without one.
#[derive(Debug, Clone)]
pub struct Mem {
    ledger: Arc<Ledger>,
}

impl Mem {
    /// A fresh ledger with `limits.max_alloc` bytes.
    pub fn new(limits: &Limits) -> Self {
        Self::with_max(limits.max_alloc)
    }

    /// A fresh ledger with `max` bytes.
    pub fn with_max(max: u64) -> Self {
        Self {
            ledger: Arc::new(Ledger::new(max)),
        }
    }

    /// What may still be charged.
    pub fn left(&self) -> u64 {
        self.ledger
            .max
            .saturating_sub(self.ledger.used.load(Ordering::Relaxed))
    }

    /// The bytes charged now.
    pub fn used(&self) -> u64 {
        self.ledger.used.load(Ordering::Relaxed)
    }

    /// The most bytes charged at once so far.
    pub fn peak(&self) -> u64 {
        self.ledger.peak.load(Ordering::Relaxed)
    }

    /// Books `n` bytes until the returned [`Held`] drops.
    ///
    /// # Errors
    /// [`Error::Alloc`] naming `n` and what was left.
    pub fn charge(&self, n: u64) -> Result<Held, Error> {
        self.ledger.reserve(n)?;
        Ok(Held {
            ledger: Arc::clone(&self.ledger),
            n,
        })
    }

    /// A charge of 0 bytes, which [`Held::grow_by`] grows.
    pub fn hold(&self) -> Held {
        Held {
            ledger: Arc::clone(&self.ledger),
            n: 0,
        }
    }

    /// `n` zero bytes, charged before they are allocated. The [`Held`] is their charge.
    ///
    /// # Errors
    /// [`Error::Alloc`] if the ledger refuses `n`, [`Error::OutOfMemory`] if the allocator does.
    pub fn alloc_zeroed(&self, n: usize) -> Result<(Vec<u8>, Held), Error> {
        let held = self.charge(n as u64)?;
        let mut v = Vec::new();
        v.try_reserve_exact(n)?;
        v.resize(n, 0);
        Ok((v, held))
    }

    /// A charged copy of `src`, whose Go capacity is `go_cap`.
    ///
    /// # Errors
    /// As [`Mem::alloc_zeroed`].
    pub fn bytes_from(&self, src: &[u8], go_cap: usize) -> Result<Bytes, Error> {
        let held = self.charge(src.len() as u64)?;
        let mut v = Vec::new();
        v.try_reserve_exact(src.len())?;
        v.extend_from_slice(src);
        Ok(Bytes::new(v, go_cap, Some(held)))
    }
}

/// Bytes charged in a [`Ledger`], released on drop.
#[derive(Debug)]
pub struct Held {
    ledger: Arc<Ledger>,
    n: u64,
}

impl Held {
    /// The bytes this charge holds.
    pub fn bytes(&self) -> u64 {
        self.n
    }

    /// Releases all but `n` bytes (nested crates are charged for their limit up front, then the
    /// charge shrinks to the real output). Does nothing if `n` is not smaller.
    pub fn shrink_to(&mut self, n: u64) {
        if n < self.n {
            self.ledger.release(self.n - n);
            self.n = n;
        }
    }

    /// Charges `extra` more bytes onto this charge (an estimate that accumulates over a parse).
    ///
    /// # Errors
    /// [`Error::Alloc`] naming `extra` and what was left; the charge is unchanged.
    pub fn grow_by(&mut self, extra: u64) -> Result<(), Error> {
        self.ledger.reserve(extra)?;
        self.n = self.n.saturating_add(extra);
        Ok(())
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.ledger.release(self.n);
    }
}

struct Charged {
    v: Vec<u8>,
    go_cap: usize,
    _held: Option<Held>,
}

/// An input-sized buffer and its charge. Clones share both, as Go's slices share their backing
/// array, so a clone charges nothing twice; the charge is released when the last clone drops.
/// `go_cap` is the capacity Go's slice would have (see `gocap`), which the cumulative decode
/// budget charges instead of the length.
#[derive(Clone)]
pub struct Bytes(Arc<Charged>);

impl Bytes {
    /// Wraps `v`, whose memory the caller has charged as `held` (`None` for a buffer that is not
    /// input-sized). A `go_cap` below `v.len()` is raised to it.
    pub fn new(v: Vec<u8>, go_cap: usize, held: Option<Held>) -> Self {
        let go_cap = go_cap.max(v.len());
        Bytes(Arc::new(Charged {
            v,
            go_cap,
            _held: held,
        }))
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0.v
    }

    pub fn len(&self) -> usize {
        self.0.v.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.v.is_empty()
    }

    /// Go's `cap(slice)` for this buffer.
    pub fn go_cap(&self) -> usize {
        self.0.go_cap
    }

    /// Whether both are the same buffer (Go: the two slices share their first element, once
    /// both lengths are known to be above 0).
    pub fn ptr_eq(a: &Bytes, b: &Bytes) -> bool {
        Arc::ptr_eq(&a.0, &b.0)
    }
}

impl Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0.v
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0.v
    }
}

/// Shows the sizes, never the content.
impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bytes(len {}, go_cap {})", self.len(), self.go_cap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_release_and_peak() {
        let m = Mem::with_max(100);
        let a = m.charge(60).unwrap();
        assert_eq!(m.charge(50).err(), Some(Error::Alloc { got: 50, max: 40 }));
        assert_eq!(m.used(), 60);
        assert_eq!(a.bytes(), 60);
        drop(a);
        assert_eq!(m.left(), 100);
        let _b = m.charge(100).unwrap();
        assert_eq!(m.peak(), 100);
        assert_eq!(m.charge(1).err(), Some(Error::Alloc { got: 1, max: 0 }));
    }

    #[test]
    fn overflowing_charge_is_refused() {
        let m = Mem::with_max(u64::MAX);
        let _a = m.charge(u64::MAX - 1).unwrap();
        assert!(matches!(m.charge(5), Err(Error::Alloc { got: 5, max: 1 })));
    }

    #[test]
    fn held_shrinks_and_grows() {
        let m = Mem::with_max(100);
        let mut h = m.charge(80).unwrap();
        h.shrink_to(30);
        assert_eq!((h.bytes(), m.used()), (30, 30));
        h.shrink_to(50);
        assert_eq!((h.bytes(), m.used()), (30, 30));
        h.grow_by(60).unwrap();
        assert_eq!((h.bytes(), m.used()), (90, 90));
        assert!(h.grow_by(11).is_err());
        assert_eq!((h.bytes(), m.used()), (90, 90));
        drop(h);
        assert_eq!(m.used(), 0);
        assert_eq!(m.peak(), 90);
    }

    #[test]
    fn alloc_zeroed_charges_first() {
        let m = Mem::new(&Limits {
            max_alloc: 16,
            ..Limits::default()
        });
        let (v, h) = m.alloc_zeroed(10).unwrap();
        assert_eq!(
            (v.len(), v.iter().all(|&b| b == 0), m.used()),
            (10, true, 10)
        );
        assert_eq!(
            m.alloc_zeroed(7).err(),
            Some(Error::Alloc { got: 7, max: 6 })
        );
        drop(h);
        assert_eq!(m.used(), 0);
    }

    #[test]
    fn bytes_clones_share_one_charge() {
        let m = Mem::with_max(100);
        let a = m.bytes_from(&[1, 2, 3, 4], 8).unwrap();
        assert_eq!((a.len(), a.go_cap(), m.used()), (4, 8, 4));
        let b = a.clone();
        assert_eq!(m.used(), 4);
        assert!(Bytes::ptr_eq(&a, &b));
        assert_eq!(&b[..], &[1, 2, 3, 4]);
        drop(a);
        assert_eq!(m.used(), 4);
        drop(b);
        assert_eq!(m.used(), 0);

        let c = m.bytes_from(&[1, 2], 8).unwrap();
        let d = m.bytes_from(&[1, 2], 8).unwrap();
        assert!(!Bytes::ptr_eq(&c, &d));
        assert_eq!(m.used(), 4);
    }

    #[test]
    fn go_cap_is_at_least_len() {
        let b = Bytes::new(vec![0; 10], 3, None);
        assert_eq!(b.go_cap(), 10);
        assert!(Bytes::new(Vec::new(), 0, None).is_empty());
        assert_eq!(format!("{b:?}"), "Bytes(len 10, go_cap 10)");
    }
}
