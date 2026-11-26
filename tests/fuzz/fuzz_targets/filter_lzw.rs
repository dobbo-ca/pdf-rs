#![no_main]
use libfuzzer_sys::fuzz_target;

#[path = "../common.rs"]
mod common;

// LZWDecode: the first five bytes choose Predictor, EarlyChange, Colors, BitsPerComponent and Columns (a byte of 0 leaves the key out), the rest is the stream.
fuzz_target!(|data: &[u8]| {
    common::run_filter(
        pdf::filter::LZW,
        &[
            b"Predictor".as_slice(),
            b"EarlyChange".as_slice(),
            b"Colors".as_slice(),
            b"BitsPerComponent".as_slice(),
            b"Columns".as_slice(),
        ],
        data,
    );
});
