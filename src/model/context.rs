//! `Context` (Go: pkg/pdfcpu/model/context.go) and `Version` (version.go).
//!
//! Go's `Context` embeds `*Configuration` and `*XRefTable` and also holds the read, optimize
//! and write contexts. Here the configuration is `xref.conf` (docs/design.md section 3), the
//! table is `xref`, and the optimize and write contexts are added by the packages that use
//! them.

use super::xref::XRefTable;
use crate::config::Config;
use crate::error::{Error, LimitKind};
use crate::ledger::Mem;
use std::collections::BTreeSet;
use std::fmt;

/// `Version`: the internal representation of a PDF version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Version {
    V10,
    V11,
    V12,
    V13,
    V14,
    V15,
    V16,
    V17,
    V20,
}

impl Version {
    /// `PDFVersion`: the version of a header or catalog string such as `1.7`.
    ///
    /// # Errors
    /// [`Error::Corrupt`] carrying the string if it is no version.
    pub fn parse(s: &[u8]) -> Result<Version, Error> {
        Ok(match s {
            b"1.0" => Version::V10,
            b"1.1" => Version::V11,
            b"1.2" => Version::V12,
            b"1.3" => Version::V13,
            b"1.4" => Version::V14,
            b"1.5" => Version::V15,
            b"1.6" => Version::V16,
            b"1.7" => Version::V17,
            b"2.0" => Version::V20,
            _ => return Err(Error::Corrupt(String::from_utf8_lossy(s).into_owned())),
        })
    }

    /// `PDFVersionRelaxed`: the versions that relaxed validation accepts as well.
    ///
    /// # Errors
    /// [`Error::Corrupt`] carrying the string if it is no version.
    pub fn parse_relaxed(s: &[u8]) -> Result<Version, Error> {
        match s {
            b"1.7.0" => Ok(Version::V17),
            _ => Err(Error::Corrupt(String::from_utf8_lossy(s).into_owned())),
        }
    }

    fn minor(self) -> u8 {
        match self {
            Version::V10 => 0,
            Version::V11 => 1,
            Version::V12 => 2,
            Version::V13 => 3,
            Version::V14 => 4,
            Version::V15 => 5,
            Version::V16 => 6,
            Version::V17 => 7,
            Version::V20 => 0,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if *self == Version::V20 {
            return f.write_str("2.0");
        }
        write!(f, "1.{}", self.minor())
    }
}

/// `ReadContext`: the state of reading one PDF file.
#[derive(Debug, Clone, Default)]
pub struct ReadInfo {
    /// Input file size.
    pub file_size: u64,
    /// 1 or 2 characters used for the end of line.
    pub eol_count: i64,
    pub repair_offset: i64,
    /// The file is linearized.
    pub linearized: bool,
    /// The file is a hybrid-reference file.
    pub hybrid: bool,
    /// The file is using object streams.
    pub using_object_streams: bool,
    /// The numbers of all object streams found, which need to be decoded.
    pub object_streams: BTreeSet<i64>,
    /// The file is using xref streams.
    pub using_xref_streams: bool,
    /// The numbers of all xref streams found.
    pub xref_streams: BTreeSet<i64>,
}

impl ReadInfo {
    /// `IsObjectStreamObject`: object `i` is an object stream. (All compressed objects are in
    /// one.)
    pub fn is_object_stream_object(&self, i: i64) -> bool {
        self.object_streams.contains(&i)
    }
}

/// `Context`: an environment for processing one PDF file.
#[derive(Debug)]
pub struct Context {
    pub xref: XRefTable,
    /// The read context; `None` for a document that was created, not read.
    pub read: Option<ReadInfo>,
    /// True when writing page dictionaries.
    pub writing_pages: bool,
    /// True when writing a destination within a page.
    pub dest: bool,
    /// Tracks annotation context during writing.
    pub writing_annotation: bool,
    /// The objects that optimize found to be redundant (Go's `Optimize`): `Some` for a document
    /// that was read, `None` for one that was created (`CreateContext` leaves it nil). WP 26
    /// added the field for `ensureInfoDict` and the writer; WP 24 adds the rest of Go's
    /// `OptimizationContext` where it needs it.
    pub optimize: Option<crate::write::RedundantObjs>,
}

impl Context {
    /// `NewContext`: a context for an input of `file_size` bytes, with a fresh ledger of
    /// `conf.limits.max_alloc` bytes.
    ///
    /// # Errors
    /// [`LimitKind::InputBytes`] if `max_input_bytes` is set and the input is larger.
    pub fn new(file_size: u64, conf: Config) -> Result<Context, Error> {
        Self::new_with_mem(file_size, conf, Mem::new(&conf.limits))
    }

    /// As [`Context::new`], charging the caller's ledger `mem` instead of a fresh one
    /// ([`XRefTable::new_with_mem`]).
    ///
    /// # Errors
    /// As [`Context::new`].
    pub fn new_with_mem(file_size: u64, conf: Config, mem: Mem) -> Result<Context, Error> {
        let max = conf.limits.max_input_bytes;
        if max > 0 && file_size > max {
            return Err(Error::Limit(LimitKind::InputBytes).context(format!(
                "maximum {max} bytes: PDF input size limit exceeded"
            )));
        }
        Ok(Context {
            xref: XRefTable::new_with_mem(conf, mem),
            read: Some(ReadInfo {
                file_size,
                ..ReadInfo::default()
            }),
            writing_pages: false,
            dest: false,
            writing_annotation: false,
            optimize: Some(crate::write::RedundantObjs::default()),
        })
    }

    /// `ReadFileSize`: the size of the input file, 0 if there is no read context.
    pub fn read_file_size(&self) -> u64 {
        self.read.as_ref().map_or(0, |r| r.file_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;
    use crate::object::Dict;

    fn table() -> XRefTable {
        XRefTable::new(Config::default())
    }

    #[test]
    fn version_strings_and_parse() {
        assert_eq!(Version::V10.to_string(), "1.0");
        assert_eq!(Version::V17.to_string(), "1.7");
        assert_eq!(Version::V20.to_string(), "2.0");
        assert_eq!(Version::parse(b"1.4"), Ok(Version::V14));
        assert_eq!(Version::parse(b"2.0"), Ok(Version::V20));
        assert_eq!(Version::parse(b"1.8"), Err(Error::Corrupt("1.8".into())));
        assert_eq!(Version::parse_relaxed(b"1.7.0"), Ok(Version::V17));
        assert!(Version::parse_relaxed(b"1.7").is_err());
        assert!(Version::V17 < Version::V20 && Version::V13 < Version::V14);
    }

    // Port of TestValidateVersionReturnsVersionRequirementError.
    #[test]
    fn validate_version_too_low() {
        let mut x = table();
        x.header_version = Some(Version::V13);
        let err = x
            .validate_version("dict=Example entry=Feature", Version::V14)
            .unwrap_err();
        assert_eq!(
            err,
            Error::Validation(
                "dict=Example entry=Feature: unsupported in version 1.3, requires version 1.4"
                    .into()
            )
        );
        assert_eq!(err.context("validation error").class(), "validate");
    }

    // Port of TestValidateVersionAcceptsSupportedVersions.
    #[test]
    fn validate_version_accepts_equal_and_newer() {
        for v in [Version::V14, Version::V15] {
            let mut x = table();
            x.header_version = Some(v);
            assert_eq!(x.validate_version("Example", Version::V14), Ok(()));
        }
    }

    // Port of TestValidateVersionUsesRootVersion.
    #[test]
    fn root_version_wins_only_when_later() {
        for (header, root, want) in [
            (Version::V17, Version::V20, Version::V20),
            (Version::V20, Version::V17, Version::V20),
            (Version::V17, Version::V17, Version::V17),
        ] {
            let mut x = table();
            x.header_version = Some(header);
            x.root_version = Some(root);
            assert_eq!(x.version(), Ok(want));
            assert_eq!(x.validate_version("Example", want), Ok(()));
        }
        // A root version alone counts, and neither is Go's nil dereference.
        let mut x = table();
        x.root_version = Some(Version::V15);
        assert_eq!(x.version_string().unwrap(), "1.5");
        assert!(!x.pdf20().unwrap());
        let x = table();
        assert!(matches!(x.version(), Err(Error::Panic(_))));
    }

    #[test]
    fn new_context_checks_the_input_size() {
        let conf = Config {
            limits: Limits {
                max_input_bytes: 10,
                ..Limits::default()
            },
            ..Config::default()
        };
        assert!(Context::new(10, conf).is_ok());
        let err = Context::new(11, conf).unwrap_err();
        assert!(err.is_limit_exceeded());
        assert_eq!(err.class(), "limit/objects");
        // 0 = no cap.
        let ctx = Context::new(u64::MAX, Config::default()).unwrap();
        assert_eq!(ctx.read_file_size(), u64::MAX);
        assert!(ctx.xref.table.is_empty() && !ctx.writing_pages);
        assert_eq!(ctx.xref.mem().used(), 0);
    }

    #[test]
    fn read_info_object_streams() {
        let mut r = ReadInfo::default();
        r.object_streams.insert(4);
        assert!(r.is_object_stream_object(4) && !r.is_object_stream_object(5));
    }

    #[test]
    fn context_without_read_info_has_no_file_size() {
        let mut ctx = Context::new(7, Config::default()).unwrap();
        ctx.read = None;
        assert_eq!(ctx.read_file_size(), 0);
        ctx.xref.root_dict = Some(Dict::default());
        assert!(ctx.xref.catalog().unwrap().is_empty());
        assert!(ctx.xref.pages().is_err());
    }
}
