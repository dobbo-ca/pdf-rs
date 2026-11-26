#![no_main]
use libfuzzer_sys::fuzz_target;

#[path = "../common.rs"]
mod common;

// `read_and_validate`: the read of the `read` target, then relaxed validation of every object the
// document reaches.
fuzz_target!(|data: &[u8]| {
    let cancel = common::never();
    match pdf::api::read_and_validate(data, &common::config(pdf::Cmd::Validate), &cancel) {
        Ok(mut ctx) => {
            common::walk_pages(&mut ctx, &cancel);
            common::settle(ctx);
        }
        Err(e) => common::report(&e),
    }
});
