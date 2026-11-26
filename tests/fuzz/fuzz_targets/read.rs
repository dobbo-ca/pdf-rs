#![no_main]
use libfuzzer_sys::fuzz_target;

#[path = "../common.rs"]
mod common;

// Arbitrary bytes as a PDF: the header, cross-reference tables and streams, object streams, the
// repair scanner, decryption with the empty password, and the page tree, under the TIGHT limits.
// A panic, an allocation outside the limits or a hang is the finding; `settle` also checks that
// the ledger gives every byte back.
fuzz_target!(|data: &[u8]| {
    let cancel = common::never();
    match pdf::api::read(data, &common::config(pdf::Cmd::Validate), &cancel) {
        Ok(mut ctx) => {
            common::walk_pages(&mut ctx, &cancel);
            common::settle(ctx);
        }
        Err(e) => common::report(&e),
    }
});
