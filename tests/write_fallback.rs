//! The `write_fallback` fuzz target's calls on its committed regression seeds, under the same
//! limits: each must finish (with any result).

#[path = "fuzz/common.rs"]
mod common;

/// Runs the target's calls on the seed `name`.
fn finishes(name: &str) {
    let path = format!(
        "{}/tests/fuzz/seeds/write_fallback/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let data = std::fs::read(path).unwrap();
    let cancel = common::never();
    let cfg = common::config(pdf::Cmd::Validate);
    let Ok(mut ctx) = pdf::api::read_validate_and_optimize(&data, &cfg, &cancel) else {
        return;
    };
    if let Ok(out) = pdf::api::write_context(&mut ctx, &pdf::WriteEnv::parity(), &cancel) {
        assert!(out.starts_with(b"%PDF-"), "the output has no header");
        let _ = pdf::api::read(&out, &cfg, &cancel);
    }
}

/// A repaired file whose garbled `... 0 obj` line makes object 58207065285320, so `Size` is in
/// the trillions: freeing the redundant objects looped over every number below it.
#[test]
fn huge_repaired_object_number_finishes() {
    finishes("timeout-2f3caca7626674d8b7a800e993f0fcb79baa2779");
}

/// The same loop by another route: an unclosed literal string makes the repair scan resume 6
/// bytes into each failed object, until it reads the line `ID[<BA517F1D8867462623ndobj`, whose
/// digits make object 51718867462623 (the integer 6).
#[test]
fn huge_object_number_from_a_resumed_repair_scan_finishes() {
    finishes("timeout-7db5ef4fe2bca0279a7a2273ca65b6cd2dec0f55");
}
