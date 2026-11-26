//! Image extraction (docs/design.md section 15, WP 12; Go: pkg/pdfcpu/extract.go,
//! image.go, writeImage.go).
//!
//! * [`extract`]: [`extract_image`], [`color_space_components`] and the image entry helpers.
//! * [`render`]: the renderers of `writeImage.go` (`RenderImage` and what it calls).
//! * [`png`]: the part of Go's `image/png` encoder that the renderers reach, byte for byte.
//! * [`tiff`]: the part of `github.com/hhrutter/tiff` that writes an uncompressed 8-bit CMYK or
//!   NRGBA image.
//!
//! Go hands the rendered image over as an `io.Reader`; here it is an [`Image`] with its bytes.
//! The working buffers (pixels, rows, output) are charged to the document's ledger before they
//! are allocated and released when the call returns.

pub mod extract;
pub mod png;
pub mod render;
pub mod tiff;

pub use extract::{Image, color_space_components, extract_image};

use crate::error::Error;
use crate::ledger::{Held, Mem};
use std::sync::atomic::{AtomicBool, Ordering};

// The colour space names (`model.DeviceGrayCS` and the others).
pub(crate) const DEVICE_GRAY: &[u8] = b"DeviceGray";
pub(crate) const DEVICE_RGB: &[u8] = b"DeviceRGB";
pub(crate) const DEVICE_CMYK: &[u8] = b"DeviceCMYK";
pub(crate) const CAL_GRAY: &[u8] = b"CalGray";
pub(crate) const CAL_RGB: &[u8] = b"CalRGB";
pub(crate) const LAB: &[u8] = b"Lab";
pub(crate) const ICC_BASED: &[u8] = b"ICCBased";
pub(crate) const SEPARATION: &[u8] = b"Separation";
pub(crate) const DEVICE_N: &[u8] = b"DeviceN";
pub(crate) const INDEXED: &[u8] = b"Indexed";

/// `contextutil.Check`.
pub(crate) fn check(cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// The error for a site where Go indexes past a slice and panics.
pub(crate) fn out_of_range() -> Error {
    Error::Panic("index out of range")
}

/// A growing output buffer. Its capacity is charged to the ledger before each growth, and the
/// charge is released when the buffer is turned into its `Vec`.
pub(crate) struct OutBuf {
    v: Vec<u8>,
    held: Held,
}

impl OutBuf {
    pub(crate) fn new(mem: &Mem) -> Result<OutBuf, Error> {
        Ok(OutBuf {
            v: Vec::new(),
            held: mem.charge(0)?,
        })
    }

    pub(crate) fn extend(&mut self, p: &[u8]) -> Result<(), Error> {
        let need = self
            .v
            .len()
            .checked_add(p.len())
            .ok_or(Error::OutOfMemory)?;
        let cap = self.v.capacity();
        if need > cap {
            let target = need.max(cap.saturating_mul(2)).max(4096);
            self.held.grow_by((target - cap) as u64)?;
            self.v.try_reserve_exact(target - self.v.len())?;
        }
        self.v.extend_from_slice(p);
        Ok(())
    }

    /// The bytes; the charge is released here.
    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.v
    }
}

/// A charged copy of `src`. The charge is released on return: the copy leaves the call.
pub(crate) fn copy_charged(mem: &Mem, src: &[u8]) -> Result<Vec<u8>, Error> {
    let _held = mem.charge(src.len() as u64)?;
    let mut v = Vec::new();
    v.try_reserve_exact(src.len())?;
    v.extend_from_slice(src);
    Ok(v)
}
