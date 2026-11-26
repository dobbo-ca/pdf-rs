#![no_main]
use libfuzzer_sys::fuzz_target;
use pdf::gocap::{read_from_cap, roundupsize, simulate_read_from, simulate_write};

// The capacity model of `bytes.Buffer`. Each two bytes of the input are a read length (1 to
// 1,114,113 bytes, up to 64 reads). The closed form `read_from_cap` of the capacity after
// `ReadFrom` must equal the step-by-step simulation for every way of cutting the data into
// reads; `simulate_write` must keep the length and a capacity that holds it; and `roundupsize`
// never rounds down.
fuzz_target!(|data: &[u8]| {
    let segs: Vec<usize> = data
        .chunks_exact(2)
        .take(64)
        .map(|c| 1 + usize::from(u16::from_le_bytes([c[0], c[1]])) * 17)
        .collect();
    if segs.is_empty() {
        return;
    }
    let total: usize = segs.iter().sum();
    match (
        read_from_cap(total),
        simulate_read_from(segs.iter().copied(), false),
    ) {
        (Ok(closed), Ok(sim)) => assert_eq!(closed, sim.cap(), "total {total}, reads {segs:?}"),
        (Err(_), Err(_)) => {}
        (a, b) => panic!("closed form {a:?} against simulation {b:?}"),
    }
    let w = simulate_write(segs.iter().copied()).expect("a write of a few MiB");
    assert_eq!(w.len(), total, "length after the writes");
    assert!(w.cap() >= w.len(), "capacity under length");
    for &n in &segs {
        assert!(roundupsize(n) >= n, "roundupsize({n}) rounds down");
    }
});
