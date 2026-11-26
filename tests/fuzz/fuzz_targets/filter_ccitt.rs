#![no_main]
use libfuzzer_sys::fuzz_target;

#[path = "../common.rs"]
mod common;

// CCITTFaxDecode: the first five bytes choose K, Columns, Rows, BlackIs1 and EncodedByteAlign, the rest is the stream.
fuzz_target!(|data: &[u8]| {
    common::run_filter(
        pdf::filter::CCITT_FAX,
        &[
            b"K".as_slice(),
            b"Columns".as_slice(),
            b"Rows".as_slice(),
            b"BlackIs1".as_slice(),
            b"EncodedByteAlign".as_slice(),
        ],
        data,
    );
});
