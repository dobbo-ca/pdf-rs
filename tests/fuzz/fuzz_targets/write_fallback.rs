#![no_main]
use libfuzzer_sys::fuzz_target;

#[path = "../common.rs"]
mod common;

// The write fallback: read, optimize and write the document as a whole new file (object streams,
// an xref stream, re-encryption of an encrypted input), with the pinned clock and random source
// so a crash reproduces. A file that was written is read again; whether that read succeeds is
// not asserted, a panic or a hang is the finding.
fuzz_target!(|data: &[u8]| {
    let cancel = common::never();
    let cfg = common::config(pdf::Cmd::Validate);
    let mut ctx = match pdf::api::read_validate_and_optimize(data, &cfg, &cancel) {
        Ok(ctx) => ctx,
        Err(e) => return common::report(&e),
    };
    match pdf::api::write_context(&mut ctx, &pdf::WriteEnv::parity(), &cancel) {
        Ok(out) => {
            assert!(out.starts_with(b"%PDF-"), "the output has no header");
            if let Ok(back) = pdf::api::read(&out, &cfg, &cancel) {
                common::settle(back);
            }
        }
        Err(e) => common::report(&e),
    }
    common::settle(ctx);
});
