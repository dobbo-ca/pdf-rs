#![no_main]
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;

#[path = "../common.rs"]
mod common;

// `read_validate_and_optimize`, under each of the three commands; the last byte of the input
// picks the command. The add-properties command also runs `properties_add`, the list command
// `properties`.
fuzz_target!(|data: &[u8]| {
    let cancel = common::never();
    let cmd = match data.last().copied().unwrap_or(0) % 3 {
        0 => pdf::Cmd::Validate,
        1 => pdf::Cmd::AddProperties,
        _ => pdf::Cmd::ListProperties,
    };
    let cfg = common::config(cmd);
    if cmd == pdf::Cmd::ListProperties {
        match pdf::api::properties(data, &cfg, &cancel) {
            Ok(_) => {}
            Err(e) => common::report(&e),
        }
        return;
    }
    match pdf::api::read_validate_and_optimize(data, &cfg, &cancel) {
        Ok(mut ctx) => {
            if cmd == pdf::Cmd::AddProperties {
                let props = BTreeMap::from([
                    ("Title".to_string(), "fuzz".to_string()),
                    ("Key".to_string(), "Wert \u{fc}".to_string()),
                ]);
                let env = pdf::WriteEnv::parity();
                let _ = pdf::api::properties_add(&mut ctx, &props, &env, &cancel);
            }
            common::walk_pages(&mut ctx, &cancel);
            common::settle(ctx);
        }
        Err(e) => common::report(&e),
    }
});
