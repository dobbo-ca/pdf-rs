//! A model of Go's slice capacity, because the cumulative decode budget charges `cap(sd.Content)`
//! and not its length (model/xreftable.go:2335; docs/design.md sections 1 and 8).
//!
//! The model is derived from Go 1.27.4's `bytes/buffer.go` (`grow`, `growSlice`, `Write`,
//! `ReadFrom`), `runtime/msize.go` (`roundupsize`) and `internal/runtime/gc/sizeclasses.go`. It
//! tracks only a length and a capacity, never the bytes. A `bytes.Buffer` here never has data
//! read out of it, so Go's `off` field is always 0 and is left out.
//!
//! Where Go cannot allocate (`ErrTooLarge`) it panics; the model returns
//! [`Error::Panic`](crate::Error::Panic), which the port's callers treat as Go's recovered
//! panic.

use crate::error::Error;

/// `gc.SizeClassToSize` (internal/runtime/gc/sizeclasses.go): the 68 malloc size classes.
const SIZE_CLASSES: [u16; 68] = [
    0, 8, 16, 24, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192, 208, 224, 240, 256, 288, 320,
    352, 384, 416, 448, 480, 512, 576, 640, 704, 768, 896, 1024, 1152, 1280, 1408, 1536, 1792,
    2048, 2304, 2688, 3072, 3200, 3456, 4096, 4864, 5376, 6144, 6528, 6784, 6912, 8192, 9472, 9728,
    10240, 10880, 12288, 13568, 14336, 16384, 18432, 19072, 20480, 21760, 24576, 27264, 28672,
    32768,
];

/// `gc.MaxSmallSize`.
const MAX_SMALL_SIZE: usize = 32768;
/// `gc.MallocHeaderSize`.
const MALLOC_HEADER_SIZE: usize = 8;
/// `gc.PageSize`: `1 << gc.PageShift`, with `PageShift` 13.
const PAGE_SIZE: usize = 8192;
/// `bytes.smallBufferSize`.
const SMALL_BUFFER_SIZE: usize = 64;
/// `bytes.MinRead`.
const MIN_READ: usize = 512;
/// `bytes.maxInt`.
const MAX_INT: usize = isize::MAX as usize;

/// `runtime.roundupsize(size, noscan=true)`: the size of the block Go's allocator gives for a
/// `[]byte` of `size` bytes. A small size goes to the smallest size class that holds it; a large
/// one to the next page. A byte slice has no pointers, so no malloc header is added.
pub fn roundupsize(size: usize) -> usize {
    if size <= MAX_SMALL_SIZE - MALLOC_HEADER_SIZE {
        // The two lookup tables of msize.go map a size to the smallest class that fits it.
        let i = SIZE_CLASSES.partition_point(|&c| usize::from(c) < size);
        return SIZE_CLASSES.get(i).map_or(size, |&c| usize::from(c));
    }
    match size.checked_add(PAGE_SIZE - 1) {
        Some(r) => r & !(PAGE_SIZE - 1),
        None => size,
    }
}

fn too_large() -> Error {
    Error::Panic("bytes.Buffer: too large")
}

/// The length and capacity of a Go `bytes.Buffer`, or of a plain `[]byte` that is only appended
/// to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Buffer {
    len: usize,
    cap: usize,
    /// `buf == nil`: no allocation yet. Only then does a first small write get capacity 64.
    nil: bool,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    /// A zero `bytes.Buffer`.
    pub fn new() -> Self {
        Self {
            len: 0,
            cap: 0,
            nil: true,
        }
    }

    /// A non-nil slice of length `len` and capacity `cap` (`make([]byte, len, cap)`). A `cap`
    /// below `len` is raised to it.
    pub fn from_parts(len: usize, cap: usize) -> Self {
        Self {
            len,
            cap: cap.max(len),
            nil: false,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    /// `bytes.Buffer.grow` (buffer.go:140-180): room for `n` more bytes. Returns the index where
    /// they go and sets the length to that index plus `n`.
    fn grow(&mut self, n: usize) -> Result<usize, Error> {
        let m = self.len;
        // tryGrowByReslice.
        if n <= self.cap.saturating_sub(m) {
            self.len = m + n;
            return Ok(m);
        }
        if self.nil && n <= SMALL_BUFFER_SIZE {
            self.nil = false;
            self.cap = SMALL_BUFFER_SIZE;
            self.len = n;
            return Ok(0);
        }
        let c = self.cap;
        if c / 2 >= m && n <= c / 2 - m {
            // Go slides the data down instead of allocating.
            self.len = m + n;
            return Ok(m);
        }
        let room = MAX_INT.checked_sub(c).and_then(|r| r.checked_sub(n));
        if !matches!(room, Some(r) if c <= r) {
            return Err(too_large());
        }
        // growSlice: c = len + n, and at least twice the old capacity, rounded up to a size class.
        let want = m.checked_add(n).ok_or_else(too_large)?;
        let want = want.max(c.checked_mul(2).ok_or_else(too_large)?);
        self.cap = roundupsize(want);
        self.nil = false;
        self.len = m + n;
        Ok(m)
    }

    /// `bytes.Buffer.Write` of `n` bytes (and `WriteByte` with `n` = 1).
    ///
    /// # Errors
    /// [`Error::Panic`] where Go panics with `ErrTooLarge`.
    pub fn write(&mut self, n: usize) -> Result<(), Error> {
        self.grow(n).map(|_| ())
    }
}

/// The buffer after a series of `bytes.Buffer.Write` calls of the given sizes, starting from a
/// zero buffer.
///
/// # Errors
/// [`Error::Panic`] where Go panics with `ErrTooLarge`.
pub fn simulate_write<I: IntoIterator<Item = usize>>(chunks: I) -> Result<Buffer, Error> {
    let mut b = Buffer::new();
    for n in chunks {
        b.write(n)?;
    }
    Ok(b)
}

/// The buffer after `bytes.Buffer.ReadFrom(r)` (buffer.go:213-235) on a zero buffer, where `r`
/// hands out the bytes of `segments` in order: each `Read` returns at most the rest of the
/// current segment, and at most the room the buffer has after its `grow(512)`.
///
/// `end_carried` says how `r` ends. With `false` the last `Read` after the data returns
/// `(0, io.EOF)` as a separate call, so `ReadFrom` grows once more before it. With `true` the
/// `Read` that drains the last segment returns the error with its data (Go's flate inflater and
/// `io.EOF`), and `ReadFrom` stops without that last grow. An empty `segments` is a `Read`
/// that returns `(0, err)` at once.
///
/// # Errors
/// [`Error::Panic`] where Go panics with `ErrTooLarge`.
pub fn simulate_read_from<I: IntoIterator<Item = usize>>(
    segments: I,
    end_carried: bool,
) -> Result<Buffer, Error> {
    let mut b = Buffer::new();
    let mut segs = segments.into_iter().peekable();
    let mut rest = 0usize;
    loop {
        let i = b.grow(MIN_READ)?;
        b.len = i;
        if rest == 0 {
            match segs.next() {
                Some(s) => rest = s,
                None => return Ok(b),
            }
        }
        let take = rest.min(b.cap.saturating_sub(i));
        b.len = i + take;
        rest -= take;
        if end_carried && rest == 0 && segs.peek().is_none() {
            return Ok(b);
        }
    }
}

/// The capacity after `ReadFrom` of `len` bytes from a reader that ends with a separate
/// `(0, io.EOF)` read, however it chunks the data: the smallest `512 * 2^k` that is at least
/// `len + 512`. (The first allocation is 512; each later growth doubles, and doubling is always
/// enough because the length never passes the capacity.)
///
/// # Errors
/// [`Error::Panic`] where Go panics with `ErrTooLarge`.
pub fn read_from_cap(len: usize) -> Result<usize, Error> {
    let want = len.checked_add(MIN_READ).ok_or_else(too_large)?;
    let cap = want.checked_next_power_of_two().ok_or_else(too_large)?;
    if cap > MAX_INT {
        return Err(too_large());
    }
    Ok(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundupsize_classes() {
        let cases: [(usize, usize); 20] = [
            (0, 0),
            (1, 8),
            (8, 8),
            (9, 16),
            (25, 32),
            (49, 64),
            (100, 112),
            (512, 512),
            (513, 576),
            (1016, 1024),
            (1017, 1024),
            (1025, 1152),
            (4097, 4864),
            (32760, 32768),
            (32761, 32768),
            (32768, 32768),
            (32769, 40960),
            (40960, 40960),
            (100_000, 106_496),
            (1 << 20, 1 << 20),
        ];
        for (size, want) in cases {
            assert_eq!(roundupsize(size), want, "roundupsize({size})");
        }
        assert_eq!(roundupsize(usize::MAX), usize::MAX);
    }

    #[test]
    fn roundupsize_never_shrinks_and_is_monotone() {
        let mut prev = 0;
        for size in 0..70_000usize {
            let r = roundupsize(size);
            assert!(r >= size && r >= prev, "roundupsize({size}) = {r}");
            prev = r;
        }
    }

    #[test]
    fn write_growth() {
        let b = simulate_write([10]).unwrap();
        assert_eq!((b.len(), b.cap()), (10, 64));
        let b = simulate_write([10, 54]).unwrap();
        assert_eq!((b.len(), b.cap()), (64, 64));
        // A full buffer doubles.
        let b = simulate_write([64, 1]).unwrap();
        assert_eq!((b.len(), b.cap()), (65, 128));
        // A first write over 64 is rounded to a size class, not 64.
        let b = simulate_write([100]).unwrap();
        assert_eq!((b.len(), b.cap()), (100, 112));
        let b = simulate_write([65]).unwrap();
        assert_eq!((b.len(), b.cap()), (65, 80));
        // Appending a byte at a time doubles at each full buffer.
        let b = simulate_write(std::iter::repeat_n(1, 300)).unwrap();
        assert_eq!((b.len(), b.cap()), (300, 512));
        // A non-nil empty slice does not get the 64 byte start.
        let mut z = Buffer::from_parts(0, 0);
        z.write(10).unwrap();
        assert_eq!(z.cap(), 16);
    }

    #[test]
    fn write_too_large() {
        let mut b = Buffer::new();
        assert_eq!(
            b.write(usize::MAX),
            Err(Error::Panic("bytes.Buffer: too large"))
        );
    }

    #[test]
    fn read_from_hand_traced() {
        // One 100 byte read, then (0, EOF): 512, then the final grow doubles it.
        let b = simulate_read_from([100], false).unwrap();
        assert_eq!((b.len(), b.cap()), (100, 1024));
        // The same, with the end carried by the data: no final grow.
        let b = simulate_read_from([100], true).unwrap();
        assert_eq!((b.len(), b.cap()), (100, 512));
        // Empty: one grow(512), then the end.
        let b = simulate_read_from([], false).unwrap();
        assert_eq!((b.len(), b.cap()), (0, 512));
        let b = simulate_read_from([], true).unwrap();
        assert_eq!((b.len(), b.cap()), (0, 512));
        // 1000 bytes in one segment: 512, 1024, then 2048 for the last grow.
        let b = simulate_read_from([1000], false).unwrap();
        assert_eq!((b.len(), b.cap()), (1000, 2048));
        let b = simulate_read_from([1000], true).unwrap();
        assert_eq!((b.len(), b.cap()), (1000, 1024));
        // A 32 KiB flush boundary of the flate decoder: 32768 bytes, then 1 byte with the end.
        let b = simulate_read_from([32768, 1], true).unwrap();
        assert_eq!(b.len(), 32769);
        assert_eq!(b.cap(), 65536);
    }

    #[test]
    fn read_from_closed_form_values() {
        assert_eq!(read_from_cap(0).unwrap(), 512);
        assert_eq!(read_from_cap(1).unwrap(), 1024);
        assert_eq!(read_from_cap(512).unwrap(), 1024);
        assert_eq!(read_from_cap(513).unwrap(), 2048);
        assert_eq!(read_from_cap(1536).unwrap(), 2048);
        assert_eq!(read_from_cap(1537).unwrap(), 4096);
        assert!(read_from_cap(usize::MAX).is_err());
    }

    // The closed form against the step-by-step simulation, for deterministic pseudo-random
    // segmentations (the fuzz target `gocap` of docs/design.md section 13 does the same with
    // more inputs).
    #[test]
    fn read_from_closed_form_matches_simulation() {
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..500 {
            let count = (next() % 40) as usize;
            let max_seg = match next() % 3 {
                0 => 50,
                1 => 3000,
                _ => 70_000,
            };
            let mut segs = Vec::new();
            let mut total = 0usize;
            for _ in 0..count {
                let n = (next() % max_seg) as usize;
                total += n;
                segs.push(n);
            }
            let b = simulate_read_from(segs.iter().copied(), false).unwrap();
            assert_eq!(b.len(), total);
            assert_eq!(b.cap(), read_from_cap(total).unwrap(), "segments {segs:?}");
            // The end carried with the data never allocates more than the other.
            let c = simulate_read_from(segs.iter().copied(), true).unwrap();
            assert_eq!(c.len(), total);
            assert!(c.cap() <= b.cap());
        }
    }
}
