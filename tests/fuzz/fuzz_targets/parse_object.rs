#![no_main]
use libfuzzer_sys::fuzz_target;
use pdf::fuzz_api::parse_object;
use pdf::parse::{ParseCtx, ParsedBudget};

#[path = "../common.rs"]
mod common;

// The object parser on arbitrary bytes, once plain and once with a small `MaxParsedBytes`
// counter and a recursion limit of 8. A parsed object is written back out in both forms. The
// parser must not read past its input, and the counter must not pass its limit unless the parse
// failed with the limit error.
fuzz_target!(|data: &[u8]| {
    let cancel = common::never();
    let mut line = data;
    let mut cx = ParseCtx::new(&cancel);
    match parse_object(&mut cx, &mut line, 0, None) {
        Ok(o) => {
            assert!(line.len() <= data.len(), "the parser moved backwards");
            let _ = o.pdf_string();
            let _ = o.go_string();
        }
        Err(e) => common::report(&e),
    }

    let mut budget = ParsedBudget::new(1 << 16);
    let mut line = data;
    let mut cx = ParseCtx::with_charge(&cancel, &mut budget);
    match parse_object(&mut cx, &mut line, 0, Some(8)) {
        Ok(_) => {
            assert!(line.len() <= data.len(), "the parser moved backwards");
            assert!(budget.parsed <= budget.max, "the counter passed its limit");
        }
        Err(e) => common::report(&e),
    }
});
