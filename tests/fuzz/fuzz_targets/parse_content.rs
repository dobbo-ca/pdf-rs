#![no_main]
use libfuzzer_sys::fuzz_target;
use pdf::fuzz_api::parse_content;

#[path = "../common.rs"]
mod common;

// The content stream parser that finds the resource names a page uses. The first byte picks
// `relaxed` and `pdf20`, the rest is the content stream.
fuzz_target!(|data: &[u8]| {
    let (flags, s) = match data.split_first() {
        Some((f, s)) => (*f, s),
        None => (0, data),
    };
    if let Err(e) = parse_content(s, flags & 1 != 0, flags & 2 != 0) {
        common::report(&e);
    }
});
