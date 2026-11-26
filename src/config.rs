//! Configuration and resource limits: `model.ResourceLimits` field for field
//! (model/configuration.go:493-550, defaults :573-591), the two fields every port shares
//! (`max_alloc`, `max_threads`) and `internal/streamlimit`.

use std::sync::OnceLock;

/// Default of [`Limits::max_object_bytes`]: 64 MiB.
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 64 << 20;
/// Default of [`Limits::max_stream_bytes`]: 512 MiB.
pub const DEFAULT_MAX_STREAM_BYTES: u64 = 512 << 20;
/// Default of [`Limits::max_decode_bytes`] (`filter.DefaultMaxDecodeBytes`): 512 MiB.
pub const DEFAULT_MAX_DECODE_BYTES: u64 = 512 << 20;
/// Default of [`Limits::max_image_pixels`]: 100 million.
pub const DEFAULT_MAX_IMAGE_PIXELS: u64 = 100_000_000;
/// Default of [`Limits::max_image_bytes`]: 512 MiB.
pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 512 << 20;
/// Default of [`Limits::max_object_count`]: 10 million.
pub const DEFAULT_MAX_OBJECT_COUNT: u64 = 10_000_000;
/// Default of [`Limits::max_object_stream_count`]: 1 million.
pub const DEFAULT_MAX_OBJECT_STREAM_COUNT: u64 = 1_000_000;
/// Default of [`Limits::max_object_stream_first`]: 16 MiB.
pub const DEFAULT_MAX_OBJECT_STREAM_FIRST: u64 = 16 << 20;
/// Default of [`Limits::max_xref_entries`]: 10 million.
pub const DEFAULT_MAX_XREF_ENTRIES: u64 = 10_000_000;
/// Default of [`Limits::max_recursion_depth`].
pub const DEFAULT_MAX_RECURSION_DEPTH: u64 = 100;
/// The most levels any native recursion over the object graph goes, whatever
/// [`Limits::max_recursion_depth`] says (the parser's `MAX_NESTING`). Go's stacks grow; ours is
/// fixed, and a deeper chain would overflow it. Over it the error is the same
/// `Sentinel::MaxRecursionDepth`.
pub const MAX_RECURSION_DEPTH_CEILING: u64 = 256;
/// Default of [`Limits::max_alloc`]: 4 GiB.
pub const DEFAULT_MAX_ALLOC: u64 = 4 << 30;

/// Go's `int64` maximum. Limits above it are clamped to it, since Go cannot express them.
const GO_MAX_INT64: u64 = i64::MAX as u64;

/// `streamlimit.DecodeAllowance`: the cumulative decoded-stream allowance for a resolved
/// per-stream limit. 1 GiB up to a 512 MiB limit, then twice the limit, saturating at
/// `i64::MAX`. Go's negative "disabled" case cannot be expressed in `u64`.
pub fn decode_allowance(max_decode_bytes: u64) -> u64 {
    if max_decode_bytes <= 1 << 29 {
        1 << 30
    } else if max_decode_bytes > GO_MAX_INT64 / 2 {
        GO_MAX_INT64
    } else {
        2 * max_decode_bytes
    }
}

/// Caps what one document may cost. Every entry point takes them, directly or through [`Config`]
/// or the `Context` that holds it. Where Go's field has a zero rule, the rule is kept and named
/// in the field's doc. The fields are unsigned, so Go's "negative = unlimited" for
/// `MaxDecodeBytes` cannot be expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Bytes of the input. 0 = no cap. Default 0.
    pub max_input_bytes: u64,
    /// Bytes of one indirect-object buffer, stream payloads excluded. 0 = the default (64 MiB).
    /// Only a value different from the default gives the typed
    /// [`LimitKind::ObjectBytes`](crate::LimitKind::ObjectBytes); at the default the error is the
    /// untyped `ObjectBufferLimit` sentinel, and a relaxed read skips the object
    /// (read.go:2378-2391). Default [`DEFAULT_MAX_OBJECT_BYTES`].
    pub max_object_bytes: u64,
    /// Encoded bytes of one stream read without a length. Default [`DEFAULT_MAX_STREAM_BYTES`].
    pub max_stream_bytes: u64,
    /// Decoded bytes of one stream. 0 = the default (512 MiB). Default
    /// [`DEFAULT_MAX_DECODE_BYTES`]. Read it through [`Limits::decode_limit`].
    pub max_decode_bytes: u64,
    /// The cumulative decode budget, in Go's slice capacity (see `gocap`), shared by
    /// `decode_stream` calls, object streams and images. 0 = [`decode_allowance`] of the
    /// per-stream limit. Default 0. Read it through [`Limits::total_decode_limit`].
    pub max_total_decode_bytes: u64,
    /// Pixels of one image. Default [`DEFAULT_MAX_IMAGE_PIXELS`].
    pub max_image_pixels: u64,
    /// Bytes of one decoded or rendered image buffer, raw and 4 bytes per pixel. Default
    /// [`DEFAULT_MAX_IMAGE_BYTES`].
    pub max_image_bytes: u64,
    /// The cross-reference object number range. Default [`DEFAULT_MAX_OBJECT_COUNT`].
    pub max_object_count: u64,
    /// `/N` of one object stream. Default [`DEFAULT_MAX_OBJECT_STREAM_COUNT`].
    pub max_object_stream_count: u64,
    /// Entries over all object streams of one parse. 0 = off. Default 0.
    pub max_object_stream_total_count: u64,
    /// The estimated heap of the objects parsed over one parse, by Go's constants (64 per array
    /// element, 96 per dictionary entry, 256 per dictionary; xreftable.go:2198-2202). 0 = off.
    /// Default 0.
    pub max_parsed_bytes: u64,
    /// Encoded stream bytes read over one parse. 0 = off. Default 0.
    pub max_stream_total_bytes: u64,
    /// Bytes of one object stream's `/First` prolog. Default
    /// [`DEFAULT_MAX_OBJECT_STREAM_FIRST`].
    pub max_object_stream_first: u64,
    /// Entries of one cross-reference section. Default [`DEFAULT_MAX_XREF_ENTRIES`].
    pub max_xref_entries: u64,
    /// Levels of recursive parsing and graph traversal. 0 = the default (100). Values over
    /// [`MAX_RECURSION_DEPTH_CEILING`] (256) act as 256, to protect the native stack. Default
    /// [`DEFAULT_MAX_RECURSION_DEPTH`].
    pub max_recursion_depth: u64,
    /// The peak bytes held by one document plus the call in progress, all threads together,
    /// nested crates included. A backstop above what Go's own limits allow at Go's defaults.
    /// Default [`DEFAULT_MAX_ALLOC`] (4 GiB). Over it: [`Error::Alloc`](crate::Error::Alloc).
    pub max_alloc: u64,
    /// The most threads one call uses, the calling thread included. 1 spawns no thread and
    /// touches no pool; 0 counts as 1. Default [`std::thread::available_parallelism`], read
    /// once. The output is the same for every value.
    pub max_threads: usize,
}

/// `available_parallelism`, read once: it reads cgroup files on every call.
pub fn default_threads() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: 0,
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            max_stream_bytes: DEFAULT_MAX_STREAM_BYTES,
            max_decode_bytes: DEFAULT_MAX_DECODE_BYTES,
            max_total_decode_bytes: 0,
            max_image_pixels: DEFAULT_MAX_IMAGE_PIXELS,
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            max_object_count: DEFAULT_MAX_OBJECT_COUNT,
            max_object_stream_count: DEFAULT_MAX_OBJECT_STREAM_COUNT,
            max_object_stream_total_count: 0,
            max_parsed_bytes: 0,
            max_stream_total_bytes: 0,
            max_object_stream_first: DEFAULT_MAX_OBJECT_STREAM_FIRST,
            max_xref_entries: DEFAULT_MAX_XREF_ENTRIES,
            max_recursion_depth: DEFAULT_MAX_RECURSION_DEPTH,
            max_alloc: DEFAULT_MAX_ALLOC,
            max_threads: default_threads(),
        }
    }
}

impl Limits {
    /// `Configuration.DecodeLimit` (configuration.go:554-559): the per-stream decode limit, 0
    /// resolved to the default.
    pub fn decode_limit(&self) -> u64 {
        if self.max_decode_bytes == 0 {
            DEFAULT_MAX_DECODE_BYTES
        } else {
            self.max_decode_bytes.min(GO_MAX_INT64)
        }
    }

    /// `Configuration.TotalDecodeLimit` (configuration.go:565-570): the cumulative allowance,
    /// 0 resolved to [`decode_allowance`] of [`Limits::decode_limit`].
    pub fn total_decode_limit(&self) -> u64 {
        if self.max_total_decode_bytes > 0 {
            self.max_total_decode_bytes.min(GO_MAX_INT64)
        } else {
            decode_allowance(self.decode_limit())
        }
    }

    /// `MaxObjectBytes` with its zero rule resolved.
    pub fn object_bytes_limit(&self) -> u64 {
        if self.max_object_bytes == 0 {
            DEFAULT_MAX_OBJECT_BYTES
        } else {
            self.max_object_bytes
        }
    }

    /// `XRefTable.MaxRecursionDepth` (recursion.go:108-114) with its zero rule resolved.
    pub fn recursion_depth(&self) -> u64 {
        if self.max_recursion_depth == 0 {
            DEFAULT_MAX_RECURSION_DEPTH
        } else {
            self.max_recursion_depth.min(MAX_RECURSION_DEPTH_CEILING)
        }
    }

    /// `max_threads` with 0 counted as 1.
    pub fn threads(&self) -> usize {
        self.max_threads.max(1)
    }
}

/// `model.CommandMode`, the subset byblos reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Cmd {
    #[default]
    Validate,
    AddProperties,
    ListProperties,
}

/// The one configuration of a document (docs/design.md section 3). The fixed Go settings are
/// constants, not fields: relaxed validation, `ValidateLinks=false`, `WriteObjectStream=true`,
/// `WriteXRefStream=true`, `Optimize=true`, `OptimizeResourceDicts=true`,
/// `OptimizeDuplicateContentStreams=false`, `DecodeAllStreams=false` and empty passwords
/// (configuration.go:882-896).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Config {
    pub limits: Limits,
    pub cmd: Cmd,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Port of internal/streamlimit TestDecodeAllowance. The "disabled" (-1) case is not
    // expressible in u64.
    #[test]
    fn decode_allowance_cases() {
        let cases: [(&str, u64, u64); 4] = [
            ("zero", 0, 1 << 30),
            ("default", 512 << 20, 1 << 30),
            ("raised", 2 << 30, 4 << 30),
            ("saturated", u64::MAX, i64::MAX as u64),
        ];
        for (name, input, want) in cases {
            assert_eq!(decode_allowance(input), want, "{name}");
        }
        assert_eq!(
            decode_allowance((i64::MAX as u64) / 2),
            2 * ((i64::MAX as u64) / 2)
        );
        assert_eq!(decode_allowance((i64::MAX as u64) / 2 + 1), i64::MAX as u64);
    }

    #[test]
    fn defaults_match_go() {
        let l = Limits::default();
        assert_eq!(l.max_input_bytes, 0);
        assert_eq!(l.max_object_bytes, 64 << 20);
        assert_eq!(l.max_stream_bytes, 512 << 20);
        assert_eq!(l.max_decode_bytes, 512 << 20);
        assert_eq!(l.max_total_decode_bytes, 0);
        assert_eq!(l.max_image_pixels, 100_000_000);
        assert_eq!(l.max_image_bytes, 512 << 20);
        assert_eq!(l.max_object_count, 10_000_000);
        assert_eq!(l.max_object_stream_count, 1_000_000);
        assert_eq!(l.max_object_stream_total_count, 0);
        assert_eq!(l.max_parsed_bytes, 0);
        assert_eq!(l.max_stream_total_bytes, 0);
        assert_eq!(l.max_object_stream_first, 16 << 20);
        assert_eq!(l.max_xref_entries, 10_000_000);
        assert_eq!(l.max_recursion_depth, 100);
        assert_eq!(l.max_alloc, 4 << 30);
        assert!(l.max_threads >= 1);
        assert_eq!(Config::default().cmd, Cmd::Validate);
    }

    #[test]
    fn zero_rules() {
        let mut z = Limits {
            max_decode_bytes: 0,
            max_total_decode_bytes: 0,
            max_object_bytes: 0,
            max_recursion_depth: 0,
            max_threads: 0,
            ..Limits::default()
        };
        assert_eq!(z.decode_limit(), 512 << 20);
        assert_eq!(z.total_decode_limit(), 1 << 30);
        assert_eq!(z.object_bytes_limit(), 64 << 20);
        assert_eq!(z.recursion_depth(), 100);
        z.max_recursion_depth = u64::MAX;
        assert_eq!(z.recursion_depth(), MAX_RECURSION_DEPTH_CEILING);
        assert_eq!(z.threads(), 1);

        let big = Limits {
            max_decode_bytes: 2 << 30,
            ..Limits::default()
        };
        assert_eq!(big.decode_limit(), 2 << 30);
        assert_eq!(big.total_decode_limit(), 4 << 30);

        let set = Limits {
            max_total_decode_bytes: 777,
            ..Limits::default()
        };
        assert_eq!(set.total_decode_limit(), 777);
    }
}
