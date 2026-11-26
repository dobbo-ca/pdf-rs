//! Stream dictionaries (Go: types/streamdict.go, structs, the lazy object-stream entry,
//! `ObjectStreamDict` and `XRefStreamDict`). The decode pipeline (`DecodeWithLimit`,
//! `DecodeLengthWithLimit`, the cached-content rules) and `Encode` are added by the filter
//! package (docs/design.md section 15, WP 4).
//!
//! Go embeds `Dict` in `StreamDict`, and `StreamDict` in the two variants, so their methods are
//! promoted. Here each type has the embedded value as a public field and also derefs to it.

use super::{Array, Dict, Object};
use crate::error::Error;
use crate::ledger::{Bytes, Mem};
use std::ops::{Deref, DerefMut};

// The filter names (pkg/filter/filter.go:28-39). WP 4 owns the full list in filter/mod.rs.
const FILTER_FLATE: &[u8] = b"FlateDecode";
const FILTER_DCT: &[u8] = b"DCTDecode";
const FILTER_JPX: &[u8] = b"JPXDecode";
const FILTER_JBIG2: &[u8] = b"JBIG2Decode";
const FILTER_CRYPT: &[u8] = b"Crypt";

/// `PDFFilter`: one stage of a stream's filter pipeline.
#[derive(Debug, Clone)]
pub struct PdfFilter {
    pub name: Vec<u8>,
    pub decode_parms: Option<Dict>,
}

/// `StreamDict`. Cloning shares `raw` and `content` (and their charge), as Go's struct copy
/// shares its slices; the dictionary and the pipeline are copied, as `StreamDict.Clone` does.
#[derive(Debug, Clone, Default)]
pub struct StreamDict {
    pub dict: Dict,
    pub stream_offset: i64,
    pub stream_length: Option<i64>,
    pub stream_length_obj_nr: Option<i64>,
    pub filter_pipeline: Vec<PdfFilter>,
    /// The encoded bytes; `None` is Go's `nil`.
    pub raw: Option<Bytes>,
    /// The decoded bytes; `None` is Go's `nil`.
    pub content: Option<Bytes>,
    pub is_page_content: bool,
    pub cs_components: i64,
}

impl StreamDict {
    /// `NewStreamDict`.
    pub fn new(
        dict: Dict,
        stream_offset: i64,
        stream_length: Option<i64>,
        stream_length_obj_nr: Option<i64>,
        filter_pipeline: Vec<PdfFilter>,
    ) -> StreamDict {
        StreamDict {
            dict,
            stream_offset,
            stream_length,
            stream_length_obj_nr,
            filter_pipeline,
            raw: None,
            content: None,
            is_page_content: false,
            cs_components: 0,
        }
    }

    /// `HasSoleFilterNamed`: the pipeline is exactly one filter, called `name`.
    pub fn has_sole_filter_named(&self, name: &[u8]) -> bool {
        matches!(self.filter_pipeline.as_slice(), [f] if f.name == name)
    }

    /// `Image`: `/Type /XObject` and `/Subtype /Image`.
    pub fn image(&self) -> bool {
        self.dict.type_() == Some(b"XObject".as_slice())
            && self.dict.subtype() == Some(b"Image".as_slice())
    }

    /// `nonCryptFilters`: the pipeline without a leading `Crypt`, which the security handler
    /// applies before decoding and after encoding (7.4.10).
    pub fn non_crypt_filters(&self) -> &[PdfFilter] {
        match self.filter_pipeline.split_first() {
            Some((f, rest)) if f.name == FILTER_CRYPT => rest,
            _ => &self.filter_pipeline,
        }
    }

    /// `preserveRawContent`: whether `content` is `raw` itself (no filter, a sole DCT that is not
    /// CMYK, or a sole JPX or JBIG2).
    pub fn preserve_raw_content(&self) -> bool {
        match self.non_crypt_filters() {
            [] => true,
            [f] => {
                (f.name == FILTER_DCT && self.cs_components != 4)
                    || preserve_encoded_image_filter(&f.name)
            }
            _ => false,
        }
    }
}

/// `preserveEncodedImageFilter`: JPX and JBIG2 are kept encoded.
pub fn preserve_encoded_image_filter(name: &[u8]) -> bool {
    name == FILTER_JPX || name == FILTER_JBIG2
}

/// An object-stream entry whose object is not parsed yet: Go's `LazyObjectStreamObject`. The
/// decoded stream, the parse and its cache live in the table entry (the read package); this is
/// the part that locates the object's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LazyRef {
    /// The object number of the object stream.
    pub stm: i64,
    /// Where the object starts in the decoded content.
    pub start: i64,
    /// Where it ends, or -1 for the end of the content (the last object of the stream).
    pub end: i64,
}

impl LazyRef {
    /// The object's text in the decoded `content` (the bounds checks of `GetData`).
    ///
    /// # Errors
    /// [`Error::Corrupt`] if the offsets do not fit the content.
    pub fn slice<'a>(&self, content: &'a [u8]) -> Result<&'a [u8], Error> {
        let len = i64::try_from(content.len()).unwrap_or(i64::MAX);
        let (start, end) = (self.start, self.end);
        let data = if end == -1 {
            if start < 0 || start > len {
                None
            } else {
                content.get(start as usize..)
            }
        } else if start < 0 || start > end || end > len {
            None
        } else {
            content.get(start as usize..end as usize)
        };
        data.ok_or_else(|| {
            Error::Corrupt(if end == -1 {
                format!("object stream offset {start} out of bounds")
            } else {
                format!("object stream offset range [{start}:{end}] out of bounds")
            })
        })
    }
}

/// `ObjectStreamDict`: an object stream, to read or to write.
#[derive(Debug, Clone)]
pub struct ObjectStreamDict {
    pub stream: StreamDict,
    pub prolog: Vec<u8>,
    pub obj_count: i64,
    pub first_obj_offset: i64,
    pub max_decode_bytes: i64,
    pub obj_array: Array,
    /// What `AddObject` has appended to Go's `Content` so far; [`ObjectStreamDict::finalize`]
    /// turns it, with the prolog, into `stream.content`. The writer charges it.
    pub body: Vec<u8>,
}

impl ObjectStreamDict {
    /// `NewObjectStreamDict`: `/Type /ObjStm`, `/Filter /FlateDecode`.
    pub fn new() -> ObjectStreamDict {
        let mut dict = Dict::default();
        dict.insert_name(b"Type", b"ObjStm");
        dict.insert_name(b"Filter", FILTER_FLATE);
        let pipeline = vec![PdfFilter {
            name: FILTER_FLATE.to_vec(),
            decode_parms: None,
        }];
        ObjectStreamDict {
            stream: StreamDict::new(dict, 0, None, None, pipeline),
            prolog: Vec::new(),
            obj_count: 0,
            first_obj_offset: 0,
            max_decode_bytes: 0,
            obj_array: Vec::new(),
            body: Vec::new(),
        }
    }

    /// `IndexedObject`: the object at `index` of the parsed object array.
    ///
    /// # Errors
    /// [`Error::Corrupt`] if there is none.
    pub fn indexed_object(&self, index: i64) -> Result<&Object, Error> {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.obj_array.get(i))
            .ok_or_else(|| Error::Corrupt(format!("IndexedObject({index}): object not available")))
    }

    /// `AddObject`: appends the serialized object `pdf_string` and its prolog entry
    /// `"<number> <offset>"`.
    pub fn add_object(&mut self, obj_number: i64, pdf_string: &[u8]) {
        let offset = self.body.len();
        if self.obj_count > 0 {
            self.prolog.push(b' ');
        }
        self.prolog
            .extend_from_slice(format!("{obj_number} {offset}").as_bytes());
        self.body.extend_from_slice(pdf_string);
        self.obj_count = self.obj_count.saturating_add(1);
    }

    /// `Finalize`: the content is the prolog followed by the added objects, and
    /// `first_obj_offset` is the prolog's length. The content is charged to `mem`.
    ///
    /// # Errors
    /// [`Error::Alloc`] or [`Error::OutOfMemory`].
    pub fn finalize(&mut self, mem: &Mem) -> Result<(), Error> {
        let total = self
            .prolog
            .len()
            .checked_add(self.body.len())
            .ok_or(Error::OutOfMemory)?;
        let held = mem.charge(total as u64)?;
        let mut v = Vec::new();
        v.try_reserve_exact(total)?;
        v.extend_from_slice(&self.prolog);
        v.extend_from_slice(&self.body);
        self.first_obj_offset = i64::try_from(self.prolog.len()).map_err(|_| Error::OutOfMemory)?;
        self.stream.content = Some(Bytes::new(v, total, Some(held)));
        self.body = Vec::new();
        Ok(())
    }
}

impl Default for ObjectStreamDict {
    fn default() -> Self {
        Self::new()
    }
}

/// `XRefStreamDict`: a cross-reference stream.
#[derive(Debug, Clone, Default)]
pub struct XRefStreamDict {
    pub stream: StreamDict,
    pub size: i64,
    pub objects: Vec<i64>,
    pub w: [i64; 3],
    pub previous_offset: Option<i64>,
}

macro_rules! embeds {
    ($outer:ty, $field:ident, $inner:ty) => {
        impl Deref for $outer {
            type Target = $inner;
            fn deref(&self) -> &$inner {
                &self.$field
            }
        }
        impl DerefMut for $outer {
            fn deref_mut(&mut self) -> &mut $inner {
                &mut self.$field
            }
        }
    };
}

embeds!(StreamDict, dict, Dict);
embeds!(ObjectStreamDict, stream, StreamDict);
embeds!(XRefStreamDict, stream, StreamDict);

#[cfg(test)]
mod tests {
    use super::*;

    fn sd_with(filters: &[&str], cs_components: i64) -> StreamDict {
        let pipeline = filters
            .iter()
            .map(|n| PdfFilter {
                name: n.as_bytes().to_vec(),
                decode_parms: None,
            })
            .collect();
        let mut sd = StreamDict::new(Dict::default(), 0, None, None, pipeline);
        sd.cs_components = cs_components;
        sd
    }

    #[test]
    fn preserve_raw_content_cases() {
        // Mirrors TestPreservedStreamContentDoesNotConsumeDecodeLimit's filter list, plus the
        // cases that are decoded.
        for name in ["", "Crypt", "DCTDecode", "JPXDecode", "JBIG2Decode"] {
            let sd = if name.is_empty() {
                sd_with(&[], 0)
            } else {
                sd_with(&[name], 0)
            };
            assert!(sd.preserve_raw_content(), "{name:?}");
        }
        // A leading Crypt is not part of the pipeline.
        assert!(sd_with(&["Crypt", "JPXDecode"], 0).preserve_raw_content());
        // CMYK DCT is decoded; so is anything else, and any longer pipeline.
        assert!(!sd_with(&["DCTDecode"], 4).preserve_raw_content());
        assert!(!sd_with(&["FlateDecode"], 0).preserve_raw_content());
        assert!(!sd_with(&["ASCIIHexDecode", "JPXDecode"], 0).preserve_raw_content());
        assert!(!sd_with(&["JPXDecode", "ASCIIHexDecode"], 0).preserve_raw_content());
        assert!(preserve_encoded_image_filter(b"JBIG2Decode"));
        assert!(!preserve_encoded_image_filter(b"DCTDecode"));
    }

    #[test]
    fn sole_filter_and_image() {
        let sd = sd_with(&["FlateDecode"], 0);
        assert!(sd.has_sole_filter_named(b"FlateDecode"));
        assert!(!sd.has_sole_filter_named(b"LZWDecode"));
        assert!(!sd_with(&[], 0).has_sole_filter_named(b"FlateDecode"));
        assert!(!sd_with(&["FlateDecode", "FlateDecode"], 0).has_sole_filter_named(b"FlateDecode"));

        let mut sd = sd_with(&[], 0);
        assert!(!sd.image());
        // Promoted Dict methods work through the deref.
        sd.insert_name(b"Type", b"XObject");
        assert!(!sd.image());
        sd.insert_name(b"Subtype", b"Image");
        assert!(sd.image());
    }

    #[test]
    fn clone_shares_the_buffers() {
        let mem = Mem::with_max(100);
        let raw = mem.bytes_from(b"abc", 3).unwrap();
        let mut sd = sd_with(&[], 0);
        sd.raw = Some(raw.clone());
        sd.content = Some(raw); // Content aliases Raw (preserveRawContent)
        let c = sd.clone();
        assert_eq!(mem.used(), 3);
        let (r, k) = (c.raw.as_ref().unwrap(), c.content.as_ref().unwrap());
        assert!(Bytes::ptr_eq(r, k));
        assert!(Bytes::ptr_eq(r, sd.raw.as_ref().unwrap()));
        drop((sd, c));
        assert_eq!(mem.used(), 0);
    }

    #[test]
    fn lazy_ref_bounds() {
        let content = b"0123456789";
        let l = |start, end| LazyRef { stm: 1, start, end };
        assert_eq!(l(2, 5).slice(content).ok(), Some(b"234".as_slice()));
        assert_eq!(l(4, -1).slice(content).ok(), Some(b"456789".as_slice()));
        assert_eq!(l(10, -1).slice(content).ok(), Some(b"".as_slice()));
        assert_eq!(l(5, 5).slice(content).ok(), Some(b"".as_slice()));
        assert_eq!(
            l(11, -1).slice(content).err(),
            Some(Error::Corrupt(
                "object stream offset 11 out of bounds".into()
            ))
        );
        assert_eq!(
            l(-2, -1).slice(content).err(),
            Some(Error::Corrupt(
                "object stream offset -2 out of bounds".into()
            ))
        );
        for (s, e) in [(-1, 3), (6, 5), (2, 11)] {
            assert_eq!(
                l(s, e).slice(content).err(),
                Some(Error::Corrupt(format!(
                    "object stream offset range [{s}:{e}] out of bounds"
                )))
            );
        }
    }

    #[test]
    fn object_stream_new_add_and_finalize() {
        let mut osd = ObjectStreamDict::new();
        assert_eq!(osd.dict.type_(), Some(b"ObjStm".as_slice()));
        assert_eq!(
            osd.dict.name_entry(b"Filter"),
            Some(b"FlateDecode".as_slice())
        );
        assert!(osd.has_sole_filter_named(b"FlateDecode"));

        osd.add_object(10, b"<</A 1>>");
        osd.add_object(11, b"(x)");
        assert_eq!(osd.prolog, b"10 0 11 8");
        assert_eq!(osd.obj_count, 2);

        let mem = Mem::with_max(1000);
        osd.finalize(&mem).unwrap();
        assert_eq!(osd.first_obj_offset, 9);
        let content = osd.stream.content.as_ref().unwrap();
        assert_eq!(&content[..], b"10 0 11 8<</A 1>>(x)");
        assert_eq!(mem.used(), 20);
        drop(osd);
        assert_eq!(mem.used(), 0);

        let tiny = Mem::with_max(5);
        let mut osd = ObjectStreamDict::default();
        osd.add_object(1, b"null");
        assert!(matches!(osd.finalize(&tiny), Err(Error::Alloc { .. })));
        assert!(osd.stream.content.is_none());
    }

    #[test]
    fn indexed_object_bounds() {
        let mut osd = ObjectStreamDict::new();
        osd.obj_array = vec![Object::Integer(7)];
        assert!(matches!(osd.indexed_object(0), Ok(Object::Integer(7))));
        for i in [-1, 1] {
            assert_eq!(
                osd.indexed_object(i).err(),
                Some(Error::Corrupt(format!(
                    "IndexedObject({i}): object not available"
                )))
            );
        }
    }
}
