#![no_main]
use libfuzzer_sys::fuzz_target;

#[path = "../common.rs"]
mod common;

// RunLengthDecode on arbitrary bytes.
fuzz_target!(|data: &[u8]| {
    common::run_filter(pdf::filter::RUN_LENGTH, &[], data);
});
