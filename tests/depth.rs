//! The recursive page tree walks of validation, optimization and the writer just under the
//! recursion ceiling (256) run on a 2 MiB thread without overflowing its stack, and a tree over
//! it fails with the recursion-depth error whatever `max_recursion_depth` says. A stack overflow aborts
//! the process, which fails the test. Debug builds have larger frames, so they get 16 MiB.

use pdf::{Config, Limits, Sentinel, WriteEnv, api};
use std::sync::atomic::AtomicBool;

/// A chain of `levels` `/Pages` nodes (objects 2..) with one page at the bottom.
fn chain_pdf(levels: usize) -> Vec<u8> {
    let mut objects = vec![b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()];
    for i in 2..levels + 2 {
        let parent = if i == 2 {
            String::new()
        } else {
            format!("/Parent {} 0 R", i - 1)
        };
        objects.push(
            format!("<< /Type /Pages /Count 1 /Kids [{} 0 R] {parent} >>", i + 1).into_bytes(),
        );
    }
    objects.push(
        format!(
            "<< /Type /Page /Parent {} 0 R /MediaBox [0 0 10 10] >>",
            levels + 1
        )
        .into_bytes(),
    );

    let mut f = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objects.iter().enumerate() {
        offsets.push(f.len());
        f.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        f.extend_from_slice(o);
        f.extend_from_slice(b"\nendobj\n");
    }
    let xref = f.len();
    f.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for o in offsets {
        f.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    f.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    f
}

/// Validates, optimizes and writes `data` on a thread with a small stack.
fn run(data: Vec<u8>) -> Result<(), pdf::Error> {
    let stack = if cfg!(debug_assertions) {
        16 << 20
    } else {
        2 << 20
    };
    let worker = std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            let cfg = Config {
                limits: Limits {
                    max_recursion_depth: u64::MAX,
                    ..Limits::default()
                },
                ..Config::default()
            };
            let cancel = AtomicBool::new(false);
            let mut ctx = api::read_validate_and_optimize(&data, &cfg, &cancel)?;
            api::write_context(&mut ctx, &WriteEnv::parity(), &cancel).map(|_| ())
        });
    match worker.map(|w| w.join()) {
        Ok(Ok(r)) => r,
        _ => panic!("the walk thread did not finish"),
    }
}

#[test]
fn page_tree_walks_at_the_ceiling() {
    // Just under the ceiling: every walk reaches the page. The walks count their depth from
    // different starting points (as in Go), so the margin keeps every one of them under 256.
    if let Err(e) = run(chain_pdf(250)) {
        panic!("under the ceiling: {e}");
    }
    // Well over the ceiling, with max_recursion_depth = u64::MAX: an error, not an overflow.
    match run(chain_pdf(300)) {
        Err(e) => assert!(e.is(Sentinel::MaxRecursionDepth), "{e}"),
        Ok(()) => panic!("a page tree over the ceiling was accepted"),
    }
}
