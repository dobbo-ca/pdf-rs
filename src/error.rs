//! The one error type of the crate. One variant per Go error that the port has to tell apart;
//! Go errors that satisfy two sentinels with `errors.Is` have a predicate that mirrors it. The
//! class token ([`Error::class`]) is the short stable name the parity oracle writes to `.err`
//! files (docs/design.md section 5).

use std::collections::TryReserveError;
use std::fmt;

/// Which cumulative or structural limit tripped, for [`Error::Limit`]. Every kind satisfies
/// Go's `errors.Is(err, model.ErrLimitExceeded)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    /// `MaxObjectStreamTotalCount` (xreftable.go:2188).
    ObjectStreamEntries,
    /// `ErrParsedLimitExceeded`: `MaxParsedBytes` (xreftable.go:2205).
    Parsed,
    /// `ErrObjectBytesExceeded`: `MaxObjectBytes` set away from its default (xreftable.go:2209).
    ObjectBytes,
    /// `ErrStreamTotalLimitExceeded`: `MaxStreamTotalBytes` (xreftable.go:2212).
    StreamTotal,
    /// `decodeBufferBudgetError`: the cumulative decode budget. Also satisfies
    /// `filter.ErrDecodeLimitExceeded` (xreftable.go:2257-2264). `objstm` says whether it was an
    /// object stream.
    DecodeBudget { objstm: bool },
    /// `MaxObjectCount`. Reserved: not returned today (the read path gives an untyped
    /// [`Error::Corrupt`], as Go does).
    ObjectCount,
    /// `MaxXRefEntries`. Reserved: not returned today (the read path gives an untyped
    /// [`Error::Corrupt`], as Go does).
    XRefEntries,
    /// `MaxObjectStreamCount`: `/N` of one object stream.
    /// Reserved: not returned today (the read path gives an untyped [`Error::Corrupt`], as Go
    /// does).
    ObjectStreamCount,
    /// `MaxObjectStreamFirst`: `/First` of one object stream.
    /// Reserved: not returned today (the read path gives an untyped [`Error::Corrupt`], as Go
    /// does).
    ObjectStreamFirst,
    /// `MaxStreamBytes`. Reserved: not returned today (the read path gives an untyped
    /// [`Error::Corrupt`], as Go does).
    StreamBytes,
    /// `MaxInputBytes`.
    InputBytes,
}

/// Internal Go sentinels that the read path tests with `errors.Is` and branches on exactly as Go
/// does (read.go:2155-2160, :3922-3928, :4220-4232, :1971).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sentinel {
    MissingTrailerDict,
    UnregisteredObject,
    CorruptObjectOffset,
    ObjectReadBudget,
    CircularRef,
    MaxRecursionDepth,
    TruncatedStreamMarker,
    XRefStreamIndexSizeMismatch,
    NilDereferenced,
    ReferenceDoesNotExist,
    AesTooShort,
    AesUnaligned,
    Eof,
    MissingXRefSection,
    MissingXRefEof,
    ObjectBufferLimit,
    CorruptXRefSubsection,
    NameObjectCorrupt,
    CorruptFontDict,
    /// `model.ErrExpectedDict` (dereference.go:32): an object that is not a dictionary.
    ExpectedDict,
    /// `model.ErrMissingEncryptDictObject` (xreftable.go:52).
    MissingEncryptDictObject,
    /// `model.ErrWrongTypeEncryptDictObject` (xreftable.go:55).
    WrongTypeEncryptDictObject,
    /// `model.ErrPageNotFound` (xreftable.go:49).
    PageNotFound,
    /// `model.ErrPageTreeCycle` (recursion.go:69).
    PageTreeCycle,
    /// `model.ErrPageTreeDuplicate` (recursion.go:72).
    PageTreeDuplicate,
    /// `errPageContentCorrupt` (parseContent.go:29).
    PageContentCorrupt,
    /// `errTJExpressionCorrupt` (parseContent.go:30).
    TJExpressionCorrupt,
    /// `errBIExpressionCorrupt` (parseContent.go:31).
    BIExpressionCorrupt,
    /// `model.ErrNameTreeCycle` (recursion.go:58).
    NameTreeCycle,
    /// `model.ErrNameTreeDuplicate` (recursion.go:61).
    NameTreeDuplicate,
    /// `model.ErrNumberTreeCycle` (recursion.go:64).
    NumberTreeCycle,
    /// `model.ErrNumberTreeDuplicate` (recursion.go:67).
    NumberTreeDuplicate,
    /// `model.ErrAnnotationIRTCycle` (recursion.go:28).
    AnnotationIrtCycle,
    /// `model.ErrStructureTreeCycle` (recursion.go:82).
    StructureTreeCycle,
    /// `model.ErrStructureTreeDuplicate` (recursion.go:85).
    StructureTreeDuplicate,
    /// `model.ErrFunctionCycle` (recursion.go:46).
    FunctionCycle,
    /// `model.ErrHalftoneCycle` (recursion.go:49).
    HalftoneCycle,
    /// `model.ErrPatternCycle` (recursion.go:76).
    PatternCycle,
    /// `model.ErrBeadCycle` (recursion.go:31).
    BeadCycle,
    /// `model.ErrMediaClipCycle` (recursion.go:55).
    MediaClipCycle,
    /// `model.ErrRenditionCycle` (recursion.go:79).
    RenditionCycle,
    /// `ErrBookmarksRepair` (outlineTree.go:28).
    BookmarksRepair,
    /// `model.ErrActionCycle` (recursion.go:25).
    ActionCycle,
    /// `model.ErrTargetCycle` (recursion.go:88).
    TargetCycle,
    /// `model.ErrColorSpaceCycle` (recursion.go:37).
    ColorSpaceCycle,
    /// `model.ErrFormFieldCycle` (recursion.go:40).
    FormFieldCycle,
    /// `model.ErrFormFieldDuplicate` (recursion.go:43).
    FormFieldDuplicate,
    /// `model.ErrCMapCycle` (recursion.go:34).
    CMapCycle,
    /// `validate.ErrMissingFont` (font.go:34).
    MissingFont,
}

impl Sentinel {
    /// Not in the oracle's `sentinels` list (main.go): Go classifies it like an untyped error.
    fn untyped_in_go(self) -> bool {
        matches!(
            self,
            Self::MissingTrailerDict
                | Self::UnregisteredObject
                | Self::ObjectReadBudget
                | Self::CircularRef
                | Self::TruncatedStreamMarker
                | Self::NilDereferenced
                | Self::AesTooShort
                | Self::AesUnaligned
                | Self::Eof
                | Self::MissingXRefSection
                | Self::MissingXRefEof
                | Self::ObjectBufferLimit
                | Self::CorruptXRefSubsection
                | Self::NameObjectCorrupt
                | Self::CorruptFontDict
        )
    }
}

/// One variant per Go error the port distinguishes. Messages carry numbers and names, never
/// document content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `pdfcpu.ErrWrongPassword` (read.go:81).
    WrongPassword,
    /// An `ErrLimitExceeded` wrap; see [`LimitKind`].
    Limit(LimitKind),
    /// `filter.ErrDecodeLimitExceeded` for one stream (filter.go:46).
    DecodeLimit,
    /// "xref subsection object range exceeds limit" (read.go:466, :480). It wraps
    /// `errCorruptXRefSubsection`, so it also satisfies [`Sentinel::CorruptXRefSubsection`].
    XRefObjectRange,
    /// An image over `max_image_pixels` (`pixels`) or `max_image_bytes` (writeImage.go:132, :145).
    ImageLimit { pixels: bool },
    /// `model.ErrNoContent` (xreftable.go:46).
    NoContent,
    /// `filter.ErrUnsupportedFilter` (filter.go:43), with the filter name.
    UnsupportedFilter(String),
    /// "invalid filter" from `NewFilter`, with the filter name.
    InvalidFilter(String),
    /// `pdfcpu.ErrUnsupportedResource` (errors.go:79).
    UnsupportedResource,
    /// Encrypt-dictionary validation refused a scheme this port does not handle (crypto.go).
    UnsupportedEncryption(String),
    /// A malformed encrypt dictionary: `errors.Join` of the causes (crypto.go:151).
    MalformedEncryption(Box<Error>),
    /// An error from the DEFLATE decoder: `flate.CorruptInputError`, `zlib.ErrHeader`,
    /// `zlib.ErrChecksum`.
    Flate(deflate::Error),
    /// `io.ErrUnexpectedEOF` that is not swallowed.
    UnexpectedEof,
    /// `lzw: invalid code`.
    Lzw,
    /// `ascii85.CorruptInputError`, with its offset.
    Ascii85(i64),
    /// An ASCII85 stream that ends without `~>` handling Go accepts.
    Ascii85Eod,
    /// `hex.InvalidByteError`, with the byte.
    Hex(u8),
    /// A CCITT error; the text is the class suffix after `ccitt/`.
    Ccitt(&'static str),
    /// A predictor error; the text is the class suffix after `predictor/` (`params`, `rows`).
    Predictor(&'static str),
    /// An error from the JPEG coefficient decoder (`image/jpeg` `FormatError` or
    /// `UnsupportedError`).
    Jpeg(jpeg::Error),
    /// An `encoding/gob` error; the text is the class suffix after `gob/`.
    Gob(&'static str),
    /// An internal sentinel, see [`Sentinel`]. Displayed as Go's `errors.New` text, with no
    /// `pdf: ` prefix.
    Sentinel(Sentinel),
    /// An error from validate.
    Validation(String),
    /// An untyped `fmt.Errorf` in read and model, displayed as Go's text with no `pdf: ` prefix.
    Corrupt(String),
    /// A site where Go panics (index out of range, nil dereference). Go's callers recover it.
    Panic(&'static str),
    /// `%w` wrapping: a message in front of the cause. Transparent to every predicate. Around
    /// [`Sentinel::ExpectedDict`] the message is Go's whole text (`dereferenceDictTypeError`
    /// puts the sentinel inside it: `obj#3 gen#0: expected types.Dict, got types.Integer`), and
    /// only the message is displayed.
    Context(Box<str>, Box<Error>),
    /// The ledger refused `got` bytes with `max` left (Rust only).
    Alloc { got: u64, max: u64 },
    /// The allocator refused a size that is within the limits (Rust only).
    OutOfMemory,
    /// The cancel flag was set (Go's `c.Err()`).
    Cancelled,
}

impl Error {
    /// Puts `msg` in front of `self`, like `fmt.Errorf("msg: %w", err)`.
    pub fn context(self, msg: impl Into<Box<str>>) -> Error {
        Error::Context(msg.into(), Box::new(self))
    }

    /// The error under every [`Error::Context`] wrap.
    pub fn root(&self) -> &Error {
        let mut e = self;
        while let Error::Context(_, inner) = e {
            e = inner;
        }
        e
    }

    /// `errors.Is(err, model.ErrLimitExceeded)`.
    pub fn is_limit_exceeded(&self) -> bool {
        matches!(self.root(), Error::Limit(_))
    }

    /// `errors.Is(err, filter.ErrDecodeLimitExceeded)`: one stream over its limit, or the
    /// cumulative budget.
    pub fn is_decode_limit_exceeded(&self) -> bool {
        matches!(
            self.root(),
            Error::DecodeLimit | Error::Limit(LimitKind::DecodeBudget { .. })
        )
    }

    /// `errors.Is(err, pdfcpu.ErrWrongPassword)`.
    pub fn is_wrong_password(&self) -> bool {
        matches!(self.root(), Error::WrongPassword)
    }

    /// `errors.Is(err, <sentinel>)`. An [`Error::XRefObjectRange`] is also a
    /// `CorruptXRefSubsection`, and the typed `ObjectBytes` limit is also an `ObjectBufferLimit`
    /// (read.go:2383).
    pub fn is(&self, s: Sentinel) -> bool {
        match self.root() {
            Error::Sentinel(t) => *t == s,
            Error::XRefObjectRange => s == Sentinel::CorruptXRefSubsection,
            Error::Limit(LimitKind::ObjectBytes) => s == Sentinel::ObjectBufferLimit,
            _ => false,
        }
    }

    /// Whether a [`Error::Context`] in the chain is `api.validationError`'s "validation error".
    fn in_validation(&self) -> bool {
        let mut e = self;
        while let Error::Context(m, inner) = e {
            if m.contains("validation error") {
                return true;
            }
            e = inner;
        }
        false
    }

    /// The short stable token the parity oracle writes to `.err` files. Assigned from the most
    /// specific case down, in the order of docs/design.md section 5.
    pub fn class(&self) -> &'static str {
        match self.root() {
            Error::Context(..) => "other",
            Error::WrongPassword => "wrong-password",
            Error::Limit(k) => match k {
                LimitKind::ObjectStreamEntries => "limit/objstm-entries",
                LimitKind::Parsed => "limit/parsed",
                LimitKind::ObjectBytes => "limit/object-bytes",
                LimitKind::StreamTotal => "limit/stream-total",
                LimitKind::DecodeBudget { .. } => "limit/total-decode",
                LimitKind::ObjectCount
                | LimitKind::XRefEntries
                | LimitKind::ObjectStreamCount
                | LimitKind::ObjectStreamFirst
                | LimitKind::StreamBytes
                | LimitKind::InputBytes => "limit/objects",
            },
            Error::DecodeLimit => "limit/decode",
            Error::XRefObjectRange => "limit/xref-range",
            Error::ImageLimit { pixels: true } => "limit/image-pixels",
            Error::ImageLimit { pixels: false } => "limit/image-bytes",
            Error::NoContent => "no-content",
            Error::UnsupportedFilter(_) => "unsupported-filter",
            Error::InvalidFilter(_) => "invalid-filter",
            Error::UnsupportedResource => "unsupported-resource",
            Error::UnsupportedEncryption(_) => "encryption/unsupported",
            Error::MalformedEncryption(_) => "encryption/malformed",
            Error::Flate(e) => match e {
                deflate::Error::Corrupt(_) => "flate/corrupt",
                deflate::Error::UnexpectedEof => "unexpected-eof",
                deflate::Error::Internal(_) => "flate/internal",
                deflate::Error::Header => "zlib/header",
                deflate::Error::Checksum => "flate/checksum",
                deflate::Error::Dictionary => "zlib/dictionary",
                deflate::Error::Io(_) => "io",
                deflate::Error::OutputLimit => "limit/decode",
                deflate::Error::AllocLimit => "limit/alloc",
                deflate::Error::OutOfMemory => "oom",
                deflate::Error::Cancelled => "cancelled",
                deflate::Error::InvalidArgument => "flate/invalid-argument",
            },
            Error::UnexpectedEof => "unexpected-eof",
            Error::Lzw => "lzw/invalid-code",
            Error::Ascii85(_) => "ascii85/corrupt",
            Error::Ascii85Eod => "ascii85/eod",
            Error::Hex(_) => "hex/invalid-byte",
            Error::Ccitt(_) => "ccitt",
            Error::Predictor(s) => match *s {
                "rows" => "predictor/rows",
                _ => "predictor/params",
            },
            Error::Jpeg(e) => match e {
                jpeg::Error::Unsupported | jpeg::Error::UnsupportedFeature(_) => "jpeg/unsupported",
                jpeg::Error::AllocLimit { .. } => "limit/alloc",
                jpeg::Error::OutOfMemory => "oom",
                jpeg::Error::Cancelled => "cancelled",
                _ => "jpeg/format",
            },
            Error::Gob(_) => "gob",
            // Go's `classify` (main.go) keeps its listed sentinels `syntax`; any other untyped
            // error is `validate` only under `api.validationError`'s "validation error" prefix,
            // whichever variant Rust gives it.
            Error::Sentinel(s) if !s.untyped_in_go() => "syntax",
            Error::Sentinel(_) | Error::Corrupt(_) | Error::Validation(_) => {
                if self.in_validation() {
                    "validate"
                } else {
                    "syntax"
                }
            }
            Error::Panic(_) => "panic",
            Error::Alloc { .. } => "limit/alloc",
            Error::OutOfMemory => "oom",
            Error::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ObjectStreamEntries => "object stream entries over MaxObjectStreamTotalCount",
            Self::Parsed => "parsed objects over MaxParsedBytes",
            Self::ObjectBytes => "object over MaxObjectBytes",
            Self::StreamTotal => "stream bytes read over MaxStreamTotalBytes",
            Self::DecodeBudget { objstm: true } => {
                "object stream decoding over the cumulative budget"
            }
            Self::DecodeBudget { objstm: false } => "stream decoding over the cumulative budget",
            Self::ObjectCount => "object count over MaxObjectCount",
            Self::XRefEntries => "xref entries over MaxXRefEntries",
            Self::ObjectStreamCount => "object stream /N over MaxObjectStreamCount",
            Self::ObjectStreamFirst => "object stream /First over MaxObjectStreamFirst",
            Self::StreamBytes => "stream over MaxStreamBytes",
            Self::InputBytes => "input over MaxInputBytes",
        })
    }
}

impl fmt::Display for Sentinel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingTrailerDict => "missing trailer dict",
            Self::UnregisteredObject => "unregistered object",
            Self::CorruptObjectOffset => "corrupt object offset",
            Self::ObjectReadBudget => "object read budget exceeded",
            Self::CircularRef => "circular object reference",
            Self::MaxRecursionDepth => "max recursion depth exceeded",
            Self::TruncatedStreamMarker => "truncated stream marker",
            Self::XRefStreamIndexSizeMismatch => "parse: xref stream Index exceeds Size",
            Self::NilDereferenced => "nil dereferenced object",
            Self::ReferenceDoesNotExist => "referenced object does not exist",
            Self::AesTooShort => "ciphertext too short",
            Self::AesUnaligned => "ciphertext not a multiple of block size",
            Self::Eof => "EOF",
            Self::MissingXRefSection => "can't detect last xref section",
            Self::MissingXRefEof => "no matching %%EOF for startxref",
            Self::ObjectBufferLimit => "object buffer limit exceeded",
            Self::CorruptXRefSubsection => "corrupt xref subsection",
            Self::NameObjectCorrupt => "parse: corrupt name object",
            Self::CorruptFontDict => "corrupt fontDict",
            Self::ExpectedDict => "expected types.Dict",
            Self::MissingEncryptDictObject => "missing encryption dictionary object",
            Self::WrongTypeEncryptDictObject => "wrong type for encryption dictionary object",
            Self::PageNotFound => "page not found",
            Self::PageTreeCycle => "circular page tree",
            Self::PageTreeDuplicate => "duplicate page tree node",
            Self::PageContentCorrupt => "corrupt page content",
            Self::TJExpressionCorrupt => "corrupt TJ expression",
            Self::BIExpressionCorrupt => "corrupt BI expression",
            Self::NameTreeCycle => "circular name tree",
            Self::NameTreeDuplicate => "duplicate name tree node",
            Self::NumberTreeCycle => "circular number tree",
            Self::NumberTreeDuplicate => "duplicate number tree node",
            Self::AnnotationIrtCycle => "circular annotation IRT chain",
            Self::StructureTreeCycle => "circular structure tree",
            Self::StructureTreeDuplicate => "duplicate structure tree node",
            Self::FunctionCycle => "circular function graph",
            Self::HalftoneCycle => "circular halftone graph",
            Self::PatternCycle => "circular Pattern graph",
            Self::BeadCycle => "circular bead chain",
            Self::MediaClipCycle => "circular MediaClip chain",
            Self::RenditionCycle => "circular rendition graph",
            Self::BookmarksRepair => "bookmarks repair failed",
            Self::ActionCycle => "circular action chain",
            Self::TargetCycle => "circular embedded target chain",
            Self::ColorSpaceCycle => "circular colour-space graph",
            Self::FormFieldCycle => "circular form field tree",
            Self::FormFieldDuplicate => "duplicate form field node",
            Self::CMapCycle => "circular CMap chain",
            Self::MissingFont => "missing font dict",
        })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongPassword => f.write_str("pdf: wrong password"),
            Self::Limit(k) => write!(f, "pdf: resource limit exceeded: {k}"),
            Self::DecodeLimit => f.write_str("pdf: decoded stream exceeds the decode limit"),
            Self::XRefObjectRange => f.write_str("pdf: xref subsection object range exceeds limit"),
            Self::ImageLimit { pixels: true } => f.write_str("pdf: image over the pixel limit"),
            Self::ImageLimit { pixels: false } => f.write_str("pdf: image over the byte limit"),
            Self::NoContent => f.write_str("pdf: no content"),
            Self::UnsupportedFilter(n) => write!(f, "pdf: unsupported filter {n}"),
            Self::InvalidFilter(n) => write!(f, "pdf: invalid filter {n}"),
            Self::UnsupportedResource => f.write_str("pdf: unsupported resource"),
            Self::UnsupportedEncryption(s) => write!(f, "pdf: unsupported encryption: {s}"),
            Self::MalformedEncryption(e) => write!(f, "pdf: malformed encryption: {e}"),
            Self::Flate(e) => write!(f, "pdf: {e}"),
            Self::UnexpectedEof => f.write_str("pdf: unexpected EOF"),
            Self::Lzw => f.write_str("pdf: lzw: invalid code"),
            Self::Ascii85(off) => write!(f, "pdf: ascii85: corrupt input at byte {off}"),
            Self::Ascii85Eod => f.write_str("pdf: ascii85: bad end of data"),
            Self::Hex(b) => write!(f, "pdf: invalid hex byte {b:#04x}"),
            Self::Ccitt(s) => write!(f, "pdf: ccitt: {s}"),
            Self::Predictor(s) => write!(f, "pdf: predictor: {s}"),
            Self::Jpeg(e) => write!(f, "pdf: {e}"),
            Self::Gob(s) => write!(f, "pdf: gob: {s}"),
            Self::Sentinel(s) => write!(f, "{s}"),
            Self::Validation(s) => write!(f, "pdf: validation: {s}"),
            Self::Corrupt(s) => f.write_str(s),
            Self::Panic(s) => write!(f, "pdf: panic: {s}"),
            Self::Context(m, e) if matches!(**e, Self::Sentinel(Sentinel::ExpectedDict)) => {
                f.write_str(m)
            }
            Self::Context(m, e) => write!(f, "{m}: {e}"),
            Self::Alloc { got, max } => {
                write!(f, "pdf: memory limit exceeded: {got} bytes with {max} left")
            }
            Self::OutOfMemory => f.write_str("pdf: out of memory"),
            Self::Cancelled => f.write_str("pdf: cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<TryReserveError> for Error {
    fn from(_: TryReserveError) -> Self {
        Error::OutOfMemory
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicates_mirror_errors_is() {
        let b = Error::Limit(LimitKind::DecodeBudget { objstm: true });
        assert!(b.is_limit_exceeded() && b.is_decode_limit_exceeded());
        assert_eq!(b.class(), "limit/total-decode");

        assert!(!Error::DecodeLimit.is_limit_exceeded());
        assert!(Error::DecodeLimit.is_decode_limit_exceeded());
        assert_eq!(Error::DecodeLimit.class(), "limit/decode");

        let p = Error::Limit(LimitKind::Parsed);
        assert!(p.is_limit_exceeded() && !p.is_decode_limit_exceeded());

        assert!(Error::XRefObjectRange.is(Sentinel::CorruptXRefSubsection));
        assert!(!Error::XRefObjectRange.is_limit_exceeded());
        assert!(Error::Limit(LimitKind::ObjectBytes).is(Sentinel::ObjectBufferLimit));
        assert!(!Error::Limit(LimitKind::Parsed).is(Sentinel::ObjectBufferLimit));
        assert!(Error::Sentinel(Sentinel::Eof).is(Sentinel::Eof));
        assert!(!Error::Sentinel(Sentinel::Eof).is(Sentinel::CircularRef));
    }

    #[test]
    fn context_is_transparent() {
        let e = Error::WrongPassword.context("read").context("api");
        assert!(e.is_wrong_password());
        assert_eq!(e.class(), "wrong-password");
        assert_eq!(e.to_string(), "api: read: pdf: wrong password");
        let l = Error::Limit(LimitKind::StreamTotal).context("x");
        assert!(l.is_limit_exceeded());
        assert_eq!(l.class(), "limit/stream-total");
    }

    #[test]
    fn sentinel_display_is_go_text() {
        for (s, go) in [
            (Sentinel::MissingTrailerDict, "missing trailer dict"),
            (Sentinel::UnregisteredObject, "unregistered object"),
            (Sentinel::CorruptObjectOffset, "corrupt object offset"),
            (Sentinel::ObjectReadBudget, "object read budget exceeded"),
            (Sentinel::CircularRef, "circular object reference"),
            (Sentinel::MaxRecursionDepth, "max recursion depth exceeded"),
            (Sentinel::TruncatedStreamMarker, "truncated stream marker"),
            (
                Sentinel::XRefStreamIndexSizeMismatch,
                "parse: xref stream Index exceeds Size",
            ),
            (Sentinel::NilDereferenced, "nil dereferenced object"),
            (
                Sentinel::ReferenceDoesNotExist,
                "referenced object does not exist",
            ),
            (Sentinel::AesTooShort, "ciphertext too short"),
            (
                Sentinel::AesUnaligned,
                "ciphertext not a multiple of block size",
            ),
            (Sentinel::Eof, "EOF"),
            (
                Sentinel::MissingXRefSection,
                "can't detect last xref section",
            ),
            (Sentinel::MissingXRefEof, "no matching %%EOF for startxref"),
            (Sentinel::ObjectBufferLimit, "object buffer limit exceeded"),
            (Sentinel::CorruptXRefSubsection, "corrupt xref subsection"),
            (Sentinel::NameObjectCorrupt, "parse: corrupt name object"),
            (Sentinel::CorruptFontDict, "corrupt fontDict"),
            (Sentinel::ExpectedDict, "expected types.Dict"),
            (
                Sentinel::MissingEncryptDictObject,
                "missing encryption dictionary object",
            ),
            (
                Sentinel::WrongTypeEncryptDictObject,
                "wrong type for encryption dictionary object",
            ),
            (Sentinel::PageNotFound, "page not found"),
            (Sentinel::PageTreeCycle, "circular page tree"),
            (Sentinel::PageTreeDuplicate, "duplicate page tree node"),
            (Sentinel::PageContentCorrupt, "corrupt page content"),
            (Sentinel::TJExpressionCorrupt, "corrupt TJ expression"),
            (Sentinel::BIExpressionCorrupt, "corrupt BI expression"),
            (Sentinel::NameTreeCycle, "circular name tree"),
            (Sentinel::NameTreeDuplicate, "duplicate name tree node"),
            (Sentinel::NumberTreeCycle, "circular number tree"),
            (Sentinel::NumberTreeDuplicate, "duplicate number tree node"),
            (
                Sentinel::AnnotationIrtCycle,
                "circular annotation IRT chain",
            ),
            (Sentinel::StructureTreeCycle, "circular structure tree"),
            (
                Sentinel::StructureTreeDuplicate,
                "duplicate structure tree node",
            ),
            (Sentinel::FunctionCycle, "circular function graph"),
            (Sentinel::HalftoneCycle, "circular halftone graph"),
            (Sentinel::PatternCycle, "circular Pattern graph"),
            (Sentinel::BeadCycle, "circular bead chain"),
            (Sentinel::MediaClipCycle, "circular MediaClip chain"),
            (Sentinel::RenditionCycle, "circular rendition graph"),
            (Sentinel::BookmarksRepair, "bookmarks repair failed"),
            (Sentinel::ActionCycle, "circular action chain"),
            (Sentinel::TargetCycle, "circular embedded target chain"),
            (Sentinel::ColorSpaceCycle, "circular colour-space graph"),
            (Sentinel::FormFieldCycle, "circular form field tree"),
            (Sentinel::FormFieldDuplicate, "duplicate form field node"),
            (Sentinel::CMapCycle, "circular CMap chain"),
            (Sentinel::MissingFont, "missing font dict"),
        ] {
            assert_eq!(s.to_string(), go, "{s:?}");
        }
    }

    #[test]
    fn display_is_go_text() {
        let e = Error::Sentinel(Sentinel::PageNotFound);
        assert_eq!(e.to_string(), "page not found");
        assert_eq!(e.class(), "syntax");
        assert_eq!(
            Error::Corrupt("wrong type <<nil>>".into()).to_string(),
            "wrong type <<nil>>"
        );
        let d = Error::Sentinel(Sentinel::ExpectedDict)
            .context("obj#3 gen#0: expected types.Dict, got types.StreamDict")
            .context("page");
        assert!(d.is(Sentinel::ExpectedDict));
        assert_eq!(
            d.to_string(),
            "page: obj#3 gen#0: expected types.Dict, got types.StreamDict"
        );
    }

    #[test]
    fn classes() {
        assert_eq!(
            Error::ImageLimit { pixels: true }.class(),
            "limit/image-pixels"
        );
        assert_eq!(
            Error::ImageLimit { pixels: false }.class(),
            "limit/image-bytes"
        );
        assert_eq!(Error::Flate(deflate::Error::Header).class(), "zlib/header");
        assert_eq!(Error::Predictor("rows").class(), "predictor/rows");
        assert_eq!(Error::Predictor("params").class(), "predictor/params");
        assert_eq!(Error::Corrupt("x".into()).class(), "syntax");
        // Go's `validate` is the "validation error" prefix, not the Rust variant.
        let v = |e: Error| e.context("x").context("validation error").class();
        assert_eq!(v(Error::Corrupt("x".into())), "validate");
        assert_eq!(v(Error::Sentinel(Sentinel::NilDereferenced)), "validate");
        assert_eq!(v(Error::Sentinel(Sentinel::PageTreeCycle)), "syntax");
        assert_eq!(v(Error::UnexpectedEof), "unexpected-eof");
        assert_eq!(Error::Validation("x".into()).class(), "syntax");
        assert_eq!(
            Error::Sentinel(Sentinel::ObjectReadBudget).class(),
            "syntax"
        );
        assert_eq!(Error::Alloc { got: 1, max: 0 }.class(), "limit/alloc");
        assert_eq!(
            Error::from(Vec::<u8>::new().try_reserve(usize::MAX).unwrap_err()),
            Error::OutOfMemory
        );
    }
}
