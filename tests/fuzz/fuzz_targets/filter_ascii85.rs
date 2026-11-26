#![no_main]
use libfuzzer_sys::fuzz_target;

#[path = "../common.rs"]
mod common;

// ASCII85Decode on arbitrary bytes.
fuzz_target!(|data: &[u8]| {
    common::run_filter(pdf::filter::ASCII85, &[], data);
});
