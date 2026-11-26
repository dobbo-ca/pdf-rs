//! The decode pipeline and `Encode` of [`StreamDict`] (Go: types/streamdict.go:249-576). The
//! structs are in `stream.rs`; this file adds their methods, so that the work of the filter
//! package does not touch the object model's file.
//!
//! Decoding applies the non-Crypt filters in order, each over the whole output of the one
//! before, and caches the result in `content`. What the cumulative decode budget charges
//! (docs/design.md section 1, row 1) is the Go capacity of that result: [`Bytes::go_cap`].

use super::stream::preserve_encoded_image_filter;
use super::{Dict, Object, PdfFilter, StreamDict};
use crate::error::Error;
use crate::filter::{self, DCT, DEFAULT_MAX_DECODE_BYTES, Decoded, Parms, new_filter};
use crate::ledger::{Bytes, Mem};
use std::sync::atomic::AtomicBool;

fn ilen(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `parmsForFilter`: the integer entries of a filter's decode parameters, and the booleans as 0
/// and 1.
fn parms_for_filter(d: Option<&Dict>) -> Parms {
    let mut m = Parms::new();
    let Some(d) = d else {
        return m;
    };
    for (k, v) in d.iter() {
        match v {
            Object::Integer(i) => {
                m.insert(k.clone(), *i);
            }
            Object::Bool(b) => {
                m.insert(k.clone(), i64::from(*b));
            }
            _ => {}
        }
    }
    m
}

/// `fixParms`: the CCITT decoder needs `Rows`; the image's `Height` stands in for it.
fn fix_parms(f: &PdfFilter, parms: &mut Parms, dict: &Dict) -> Result<(), Error> {
    if f.name == filter::CCITT_FAX && !parms.contains_key(b"Rows".as_slice()) {
        let Some(height) = dict.int_entry(b"Height") else {
            return Err(Error::Corrupt("ccitt: \"Height\" required".into()));
        };
        parms.insert(b"Rows".to_vec(), height);
    }
    Ok(())
}

/// The prefix of an error from the stage `what` of filter `idx`: `stream filter[0] "Name": what`.
fn at(idx: usize, name: &[u8], what: &str) -> String {
    format!(
        "stream filter[{idx}] {:?}: {what}",
        String::from_utf8_lossy(name)
    )
}

impl StreamDict {
    /// `DecodeWithLimit`: decodes `raw` into `content` unless that is done, under the per-stream
    /// limit `max_decode_bytes` (0 is the default, negative is none).
    ///
    /// # Errors
    /// See [`StreamDict::decode_length_with_limit`].
    pub fn decode_with_limit(
        &mut self,
        max_decode_bytes: i64,
        mem: &Mem,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        self.decode_length_with_limit(-1, max_decode_bytes, mem, cancel)
            .map(|_| ())
    }

    /// `DecodeLengthWithLimit`: the first `max_len` bytes of the decoded stream (all of it,
    /// and cached in `content`, if `max_len` is negative). `max_decode_bytes` is the
    /// per-stream limit; a prefix over it is refused.
    ///
    /// A stream with no filter, a sole DCT that is not CMYK, or a terminal JPX or JBIG2 is
    /// not decoded: its `content` is its `raw` (shared, not copied), and the limit does not
    /// apply to it.
    ///
    /// # Errors
    /// [`Error::DecodeLimit`] over the limit; [`Error::UnexpectedEof`] for a prefix longer than
    /// the decoded stream; each filter's errors, with `stream filter[i] "Name": construct |
    /// prepare parameters | decode` as context; [`Error::UnsupportedFilter`] for JPX or JBIG2
    /// that is not the last filter; the ledger's errors; [`Error::Cancelled`].
    pub fn decode_length_with_limit(
        &mut self,
        max_len: i64,
        max_decode_bytes: i64,
        mem: &Mem,
        cancel: &AtomicBool,
    ) -> Result<Bytes, Error> {
        let limit = if max_decode_bytes == 0 {
            DEFAULT_MAX_DECODE_BYTES
        } else {
            max_decode_bytes
        };
        if max_len >= 0 && limit >= 0 && max_len > limit {
            return Err(Error::DecodeLimit);
        }
        if self.content.is_some() {
            // This stream has already been decoded.
            return self.cached_content(max_len, limit, mem);
        }

        // No filter, sole DCT except CMYK, or a terminal opaque image filter: nothing to decode
        // for consumers that can preserve the original image stream.
        if self.preserve_raw_content() {
            self.content = self.raw.clone();
            return self.cached_content(max_len, limit, mem);
        }

        self.decode_length(max_len, max_decode_bytes, mem, cancel)
    }

    /// `decodeLength`.
    fn decode_length(
        &mut self,
        max_len: i64,
        max_decode_bytes: i64,
        mem: &Mem,
        cancel: &AtomicBool,
    ) -> Result<Bytes, Error> {
        let fpl: Vec<PdfFilter> = self.non_crypt_filters().to_vec();
        // The output of the last filter that ran; before the first filter the input is `raw`.
        let mut cur: Option<Decoded> = None;

        // Apply each filter in the pipeline to the result of the preceding filter.
        for (idx, f) in fpl.iter().enumerate() {
            if preserve_encoded_image_filter(&f.name) {
                if idx != fpl.len() - 1 {
                    return Err(Error::UnsupportedFilter(
                        String::from_utf8_lossy(&f.name).into_owned(),
                    )
                    .context(at(idx, &f.name, "decode")));
                }
                break;
            }

            if f.name == DCT && self.cs_components != 4 {
                break;
            }

            let mut parms = parms_for_filter(f.decode_parms.as_ref());
            fix_parms(f, &mut parms, &self.dict)
                .map_err(|e| e.context(at(idx, &f.name, "prepare parameters")))?;

            let fi = new_filter(&f.name, parms, Some(max_decode_bytes))
                .map_err(|e| e.context(at(idx, &f.name, "construct")))?;

            let out = {
                let input: &[u8] = match &cur {
                    Some(d) => d.as_slice(),
                    None => self.raw.as_deref().unwrap_or(&[]),
                };
                if max_len >= 0 && idx == fpl.len() - 1 {
                    fi.decode_length(input, max_len, mem, cancel)
                } else {
                    fi.decode(input, mem, cancel)
                }
            }
            .map_err(|e| e.context(at(idx, &f.name, "decode")))?;
            cur = Some(out);
        }

        let Some(data) = cur else {
            return Err(Error::Corrupt(
                "copy decoded content: missing reader".into(),
            ));
        };

        if max_len < 0 {
            // A buffer that was never written to has a nil `Bytes()`: the stream is not decoded.
            let nil = data.is_nil();
            let bytes = data.into_bytes();
            self.content = if nil { None } else { Some(bytes.clone()) };
            return Ok(bytes);
        }
        if max_len > ilen(data.len()) {
            return Err(Error::UnexpectedEof);
        }
        let n = usize::try_from(max_len).map_err(|_| Error::OutOfMemory)?;
        if n == data.len() {
            return Ok(data.into_bytes());
        }
        mem.bytes_from(&data.as_slice()[..n], n)
    }

    /// `cachedContent`.
    fn cached_content(&self, max_len: i64, limit: i64, mem: &Mem) -> Result<Bytes, Error> {
        let len = self.content.as_ref().map_or(0, |c| c.len());
        if max_len < 0 {
            if limit >= 0 && ilen(len) > limit && !self.preserve_raw_content() {
                return Err(Error::DecodeLimit);
            }
            return Ok(match &self.content {
                Some(c) => c.clone(),
                None => Bytes::new(Vec::new(), 0, None),
            });
        }
        if max_len > ilen(len) {
            return Err(Error::UnexpectedEof);
        }
        let n = usize::try_from(max_len).map_err(|_| Error::OutOfMemory)?;
        match &self.content {
            Some(c) if n < c.len() => mem.bytes_from(&c[..n], n),
            Some(c) => Ok(c.clone()),
            None => Ok(Bytes::new(Vec::new(), 0, None)),
        }
    }

    /// `Encode`: applies the non-Crypt filters, last to first, to `content` to make `raw`, and
    /// sets `Length`. Only Flate encodes (docs/scope.md section 6).
    ///
    /// # Errors
    /// [`Error::UnsupportedFilter`] for any other filter, the predictor errors of Flate, the
    /// ledger's errors and [`Error::Cancelled`].
    pub fn encode(&mut self, mem: &Mem, cancel: &AtomicBool) -> Result<(), Error> {
        if self.content.is_none() && self.raw.is_some() {
            // Not decoded yet, no need to encode.
            return Ok(());
        }

        // No filter specified, nothing to encode.
        let fpl: Vec<PdfFilter> = self.non_crypt_filters().to_vec();
        if fpl.is_empty() {
            self.raw = self.content.clone();
            self.set_length(self.raw.as_ref().map_or(0, |r| r.len()));
            return Ok(());
        }

        // Apply each filter in the pipeline to the result of the preceding filter.
        let mut cur: Option<Decoded> = None;
        for f in fpl.iter().rev() {
            let parms = parms_for_filter(f.decode_parms.as_ref());
            let fi = new_filter(&f.name, parms, None)?;
            let out = {
                let input: &[u8] = match &cur {
                    Some(d) => d.as_slice(),
                    None => self.content.as_deref().unwrap_or(&[]),
                };
                fi.encode(input, mem, cancel)?
            };
            cur = Some(out);
        }

        let Some(data) = cur else {
            return Ok(());
        };
        let raw = data.into_bytes();
        let len = raw.len();
        self.raw = Some(raw);
        self.set_length(len);
        Ok(())
    }

    /// `sd.StreamLength = &n; sd.Update("Length", Integer(n))`.
    fn set_length(&mut self, len: usize) {
        let n = ilen(len);
        self.stream_length = Some(n);
        self.dict.update(b"Length", Object::Integer(n));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;
    use crate::filter::{ASCII_HEX, ASCII85, CCITT_FAX, FLATE, JBIG2, JPX, RUN_LENGTH};

    fn mem() -> Mem {
        Mem::new(&Limits::default())
    }

    fn pipeline(names: &[&str]) -> Vec<PdfFilter> {
        names
            .iter()
            .map(|n| PdfFilter {
                name: n.as_bytes().to_vec(),
                decode_parms: None,
            })
            .collect()
    }

    fn stream(mem: &Mem, raw: &[u8], names: &[&str]) -> StreamDict {
        let mut sd = StreamDict::new(Dict::default(), 0, None, None, pipeline(names));
        sd.raw = Some(mem.bytes_from(raw, raw.len()).unwrap());
        sd
    }

    fn content(sd: &StreamDict) -> Option<&[u8]> {
        sd.content.as_deref()
    }

    fn name(b: &[u8]) -> &str {
        std::str::from_utf8(b).unwrap()
    }

    #[test]
    fn decode_preserves_sole_jbig2_stream() {
        // TestDecodePreservesSoleJBIG2Stream.
        let m = mem();
        let raw = [0x97, b'J', b'B', b'2'];
        let mut sd = stream(&m, &raw, &[name(JBIG2)]);
        sd.decode_with_limit(DEFAULT_MAX_DECODE_BYTES, &m, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(content(&sd), Some(&raw[..]));
        // The content is the raw buffer itself, not a copy.
        assert!(Bytes::ptr_eq(
            sd.content.as_ref().unwrap(),
            sd.raw.as_ref().unwrap()
        ));
        assert_eq!(m.used(), 4);
    }

    #[test]
    fn decode_preserves_terminal_opaque_image_stream() {
        // TestDecodePreservesTerminalOpaqueImageStream.
        for terminal in [JPX, JBIG2] {
            let m = mem();
            let mut sd = stream(&m, b"616263>", &[name(ASCII_HEX), name(terminal)]);
            sd.decode_with_limit(DEFAULT_MAX_DECODE_BYTES, &m, &AtomicBool::new(false))
                .unwrap();
            assert_eq!(content(&sd), Some(&b"abc"[..]));
        }
    }

    #[test]
    fn decode_rejects_non_terminal_opaque_image_stream() {
        // TestDecodeRejectsNonTerminalOpaqueImageStream and
        // TestDecodePreservesUnsupportedFilterSentinel.
        let m = mem();
        let mut sd = stream(&m, b"abc", &[name(JPX), name(ASCII_HEX)]);
        let e = sd
            .decode_with_limit(DEFAULT_MAX_DECODE_BYTES, &m, &AtomicBool::new(false))
            .unwrap_err();
        assert!(matches!(e.root(), Error::UnsupportedFilter(_)));
        assert_eq!(e.class(), "unsupported-filter");
        let s = e.to_string();
        assert!(s.contains("stream filter[0]") && s.contains("JPXDecode") && s.contains("decode"));
        assert!(sd.content.is_none());
    }

    #[test]
    fn mid_pipeline_dct_that_is_not_cmyk_stops_the_pipeline() {
        let m = mem();
        let c = AtomicBool::new(false);
        // The output of the filter before it is the content.
        let mut sd = stream(&m, b"616263>", &[name(ASCII_HEX), name(DCT)]);
        sd.decode_with_limit(0, &m, &c).unwrap();
        assert_eq!(content(&sd), Some(&b"abc"[..]));
        // With nothing before it there is no reader.
        let mut sd = stream(&m, b"616263>", &[name(DCT), name(ASCII_HEX)]);
        let e = sd.decode_with_limit(0, &m, &c).unwrap_err();
        assert_eq!(
            e,
            Error::Corrupt("copy decoded content: missing reader".into())
        );
    }

    #[test]
    fn re_encodes_the_flate_predictor() {
        // TestStreamDictReencodesFlatePredictor.
        let m = mem();
        let c = AtomicBool::new(false);
        let mut parms = Dict::default();
        parms.insert_int(b"Predictor", 12);
        parms.insert_int(b"Columns", 3);
        let mut sd = StreamDict::new(
            Dict::default(),
            0,
            None,
            None,
            vec![PdfFilter {
                name: FLATE.to_vec(),
                decode_parms: Some(parms),
            }],
        );
        sd.content = Some(m.bytes_from(b"abcdef", 6).unwrap());
        sd.encode(&m, &c).unwrap();
        // Length is set, as the field and as the entry.
        let raw_len = sd.raw.as_ref().unwrap().len() as i64;
        assert_eq!(sd.stream_length, Some(raw_len));
        assert_eq!(sd.dict.int_entry(b"Length"), Some(raw_len));
        sd.content = None;
        sd.decode_with_limit(DEFAULT_MAX_DECODE_BYTES, &m, &c)
            .unwrap();
        assert_eq!(content(&sd), Some(&b"abcdef"[..]));
        sd.content = Some(m.bytes_from(b"abcdefghi", 9).unwrap());
        sd.encode(&m, &c).unwrap();
        sd.content = None;
        sd.decode_with_limit(DEFAULT_MAX_DECODE_BYTES, &m, &c)
            .unwrap();
        assert_eq!(content(&sd), Some(&b"abcdefghi"[..]));
        // Once the content is gone the old buffers are released.
        drop(sd);
        assert_eq!(m.used(), 0);
    }

    #[test]
    fn encode_without_filters_and_when_not_decoded() {
        let m = mem();
        let c = AtomicBool::new(false);
        // No filter: raw is content.
        let mut sd = StreamDict::new(Dict::default(), 0, None, None, Vec::new());
        sd.content = Some(m.bytes_from(b"hello", 5).unwrap());
        sd.encode(&m, &c).unwrap();
        assert!(Bytes::ptr_eq(
            sd.raw.as_ref().unwrap(),
            sd.content.as_ref().unwrap()
        ));
        assert_eq!(sd.stream_length, Some(5));
        assert_eq!(sd.dict.int_entry(b"Length"), Some(5));
        // Raw only (not decoded yet): nothing to do, Length untouched.
        let mut sd = stream(&m, b"xyz", &[name(FLATE)]);
        sd.encode(&m, &c).unwrap();
        assert_eq!(sd.stream_length, None);
        // A leading Crypt is not encoded.
        let mut sd = StreamDict::new(Dict::default(), 0, None, None, pipeline(&["Crypt"]));
        sd.content = Some(m.bytes_from(b"hello", 5).unwrap());
        sd.encode(&m, &c).unwrap();
        assert_eq!(sd.raw.as_deref(), Some(&b"hello"[..]));
        // Only Flate encodes.
        let mut sd = StreamDict::new(Dict::default(), 0, None, None, pipeline(&[name(ASCII85)]));
        sd.content = Some(m.bytes_from(b"hello", 5).unwrap());
        let e = sd.encode(&m, &c).unwrap_err();
        assert_eq!(e.class(), "unsupported-filter");
    }

    #[test]
    fn decode_length_rejects_oversized_prefix_without_filters() {
        // TestDecodeLengthWithLimitRejectsOversizedPrefixWithoutFilters.
        let m = mem();
        let c = AtomicBool::new(false);
        let mut raw = stream(&m, b"hello", &[]);
        assert_eq!(
            raw.decode_length_with_limit(5, 4, &m, &c).err(),
            Some(Error::DecodeLimit)
        );
        let mut cached = StreamDict::new(Dict::default(), 0, None, None, Vec::new());
        cached.content = Some(m.bytes_from(b"hello", 5).unwrap());
        assert_eq!(
            cached.decode_length_with_limit(5, 4, &m, &c).err(),
            Some(Error::DecodeLimit)
        );
    }

    #[test]
    fn cached_filter_content_honors_decode_limit() {
        // TestCachedFilterContentHonorsDecodeLimit.
        let m = mem();
        let c = AtomicBool::new(false);
        for terminal in ["", name(JPX), name(JBIG2)] {
            let mut names = vec![name(ASCII_HEX)];
            if !terminal.is_empty() {
                names.push(terminal);
            }
            let mut sd = stream(&m, b"68656c6c6f>", &names);
            sd.decode_with_limit(5, &m, &c).unwrap();
            assert_eq!(
                sd.decode_with_limit(4, &m, &c).err(),
                Some(Error::DecodeLimit),
                "terminal {terminal:?}"
            );
            let got = sd.decode_length_with_limit(4, 4, &m, &c).unwrap();
            assert_eq!(&got[..], b"hell", "terminal {terminal:?}");
            // A prefix longer than the content.
            assert_eq!(
                sd.decode_length_with_limit(6, 6, &m, &c).err(),
                Some(Error::UnexpectedEof)
            );
        }
    }

    #[test]
    fn preserved_content_does_not_consume_the_decode_limit() {
        // TestPreservedStreamContentDoesNotConsumeDecodeLimit.
        let m = mem();
        let c = AtomicBool::new(false);
        for n in ["", "Crypt", name(DCT), name(JPX), name(JBIG2)] {
            let names: Vec<&str> = if n.is_empty() { vec![] } else { vec![n] };
            let mut sd = stream(&m, b"hello", &names);
            for _ in 0..2 {
                sd.decode_with_limit(4, &m, &c).unwrap();
                assert_eq!(content(&sd), Some(&b"hello"[..]), "filter {n:?}");
            }
        }
        // CMYK DCT is not preserved: it is decoded, and "hello" is not a JPEG.
        let mut sd = stream(&m, b"hello", &[name(DCT)]);
        sd.cs_components = 4;
        assert_eq!(
            sd.decode_with_limit(4, &m, &c).unwrap_err().class(),
            "jpeg/format"
        );
    }

    #[test]
    fn raw_none_is_an_empty_preserved_stream() {
        let m = mem();
        let c = AtomicBool::new(false);
        let mut sd = StreamDict::new(Dict::default(), 0, None, None, Vec::new());
        sd.decode_with_limit(0, &m, &c).unwrap();
        assert!(sd.content.is_none());
        let b = sd.decode_length_with_limit(-1, 0, &m, &c).unwrap();
        assert!(b.is_empty());
        assert_eq!(
            sd.decode_length_with_limit(1, 0, &m, &c).err(),
            Some(Error::UnexpectedEof)
        );
    }

    #[test]
    fn prefix_decode_does_not_cache() {
        let m = mem();
        let c = AtomicBool::new(false);
        let mut sd = stream(&m, b"68656c6c6f>", &[name(ASCII_HEX)]);
        let p = sd.decode_length_with_limit(2, 0, &m, &c).unwrap();
        assert_eq!(&p[..], b"he");
        assert!(sd.content.is_none());
        // A prefix longer than the stream: ASCIIHex's error, with the filter's context.
        let e = sd.decode_length_with_limit(6, 0, &m, &c).unwrap_err();
        assert_eq!(e.root(), &Error::UnexpectedEof);
        assert!(e.to_string().contains("stream filter[0]"));
        // The whole stream is cached, and then a prefix is a copy of it.
        sd.decode_with_limit(0, &m, &c).unwrap();
        let cap = sd.content.as_ref().unwrap().go_cap();
        assert_eq!(cap, 5);
        let p = sd.decode_length_with_limit(3, 0, &m, &c).unwrap();
        assert_eq!(&p[..], b"hel");
    }

    #[test]
    fn content_capacity_is_what_go_would_have() {
        let m = mem();
        let c = AtomicBool::new(false);
        // ASCIIHex: make([]byte, n).
        let mut sd = stream(&m, b"68656c6c6f>", &[name(ASCII_HEX)]);
        sd.decode_with_limit(0, &m, &c).unwrap();
        assert_eq!(sd.content.as_ref().unwrap().go_cap(), 5);
        // Flate of 5 bytes: the one read that carries the end, 512.
        let f = new_filter(FLATE, Parms::new(), None).unwrap();
        let z = f.encode(b"hello", &m, &c).unwrap();
        let mut sd = stream(&m, z.as_slice(), &[name(FLATE)]);
        sd.decode_with_limit(0, &m, &c).unwrap();
        assert_eq!(content(&sd), Some(&b"hello"[..]));
        assert_eq!(sd.content.as_ref().unwrap().go_cap(), 512);
        // RunLength of nothing: Go's buffer was never written, so the stream is not decoded.
        let mut sd = stream(&m, &[0x80], &[name(RUN_LENGTH)]);
        sd.decode_with_limit(0, &m, &c).unwrap();
        assert!(sd.content.is_none());
    }

    #[test]
    fn errors_have_the_filter_context() {
        let m = mem();
        let c = AtomicBool::new(false);
        let check = |e: Error, idx: usize, name: &str, phase: &str| {
            let s = e.to_string();
            assert!(
                s.contains(&format!("stream filter[{idx}]"))
                    && s.contains(name)
                    && s.contains(phase),
                "{s}"
            );
        };
        // TestDecodeWrapsFilterParameterPreparation: CCITT needs Height or Rows.
        let mut sd = stream(&m, b"data", &[name(CCITT_FAX)]);
        let e = sd.decode_with_limit(0, &m, &c).unwrap_err();
        check(e, 0, "CCITTFaxDecode", "prepare parameters");
        // TestDecodeWrapsFilterConstruction.
        let mut sd = stream(&m, b"data", &["UnknownDecode"]);
        let e = sd.decode_with_limit(0, &m, &c).unwrap_err();
        assert_eq!(e.class(), "invalid-filter");
        check(e, 0, "UnknownDecode", "construct");
        // TestDecodeWrapsFilterExecution.
        let mut sd = stream(&m, b"not zlib", &[name(FLATE)]);
        let e = sd.decode_with_limit(0, &m, &c).unwrap_err();
        assert_eq!(e.root(), &Error::Flate(deflate::Error::Header));
        check(e, 0, "FlateDecode", "decode");
        // The index is the position in the pipeline.
        let mut sd = stream(&m, b"6e6f>", &[name(ASCII_HEX), name(FLATE)]);
        let e = sd.decode_with_limit(0, &m, &c).unwrap_err();
        check(e, 1, "FlateDecode", "decode");
        assert!(sd.content.is_none());
    }

    #[test]
    fn corrupt_flate_error_is_the_same_for_every_call() {
        // TestLazyObjectStreamGetDataPreservesDecodeErrorText: the decode error is not changed by
        // who asks.
        let m = mem();
        let c = AtomicBool::new(false);
        let mut a = stream(&m, b"not flate data", &[name(FLATE)]);
        let mut b = a.clone();
        let ea = a.decode_with_limit(1 << 20, &m, &c).unwrap_err();
        let eb = b.decode_with_limit(1 << 20, &m, &c).unwrap_err();
        assert_eq!(ea.to_string(), eb.to_string());
    }

    #[test]
    fn ccitt_rows_come_from_height() {
        let f = PdfFilter {
            name: CCITT_FAX.to_vec(),
            decode_parms: None,
        };
        let mut dict = Dict::default();
        let mut parms = Parms::new();
        assert!(fix_parms(&f, &mut parms, &dict).is_err());
        dict.insert_int(b"Height", 2);
        fix_parms(&f, &mut parms, &dict).unwrap();
        assert_eq!(parms.get(b"Rows".as_slice()), Some(&2));
        // An explicit Rows stays, and other filters are not touched.
        parms.insert(b"Rows".to_vec(), 7);
        fix_parms(&f, &mut parms, &dict).unwrap();
        assert_eq!(parms.get(b"Rows".as_slice()), Some(&7));
        let g = PdfFilter {
            name: FLATE.to_vec(),
            decode_parms: None,
        };
        let mut empty = Parms::new();
        fix_parms(&g, &mut empty, &Dict::default()).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn parms_for_filter_keeps_integers_and_booleans() {
        let mut d = Dict::default();
        d.insert_int(b"Predictor", 12);
        d.insert_bool(b"BlackIs1", true);
        d.insert_bool(b"EncodedByteAlign", false);
        d.insert_name(b"Other", b"x");
        d.insert_float(b"F", 1.5);
        let p = parms_for_filter(Some(&d));
        assert_eq!(p.len(), 3);
        assert_eq!(p.get(b"Predictor".as_slice()), Some(&12));
        assert_eq!(p.get(b"BlackIs1".as_slice()), Some(&1));
        assert_eq!(p.get(b"EncodedByteAlign".as_slice()), Some(&0));
        assert!(parms_for_filter(None).is_empty());
    }
}
