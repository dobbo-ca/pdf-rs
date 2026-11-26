#![no_main]
use libfuzzer_sys::fuzz_target;
use pdf::Mem;
use pdf::filter::{FLATE, Parms, new_filter};

#[path = "../common.rs"]
mod common;

// FlateDecode with the predictors: the first four bytes choose Predictor, Colors,
// BitsPerComponent and Columns, the rest is the stream. The input is then encoded with the
// in-crate Go zlib encoder and decoded again, which must give the input back.
fuzz_target!(|data: &[u8]| {
    common::run_filter(
        FLATE,
        &[
            b"Predictor".as_slice(),
            b"Colors".as_slice(),
            b"BitsPerComponent".as_slice(),
            b"Columns".as_slice(),
        ],
        data,
    );
    if data.len() > 1 << 20 {
        return;
    }
    let Ok(f) = new_filter(FLATE, Parms::new(), Some(common::decode_limit())) else {
        return;
    };
    let mem = Mem::new(&common::TIGHT);
    let cancel = common::never();
    if let Ok(enc) = f.encode(data, &mem, &cancel) {
        match f.decode(enc.as_slice(), &mem, &cancel) {
            Ok(dec) => assert_eq!(dec.as_slice(), data, "the round trip changed the data"),
            Err(e) => panic!("the encoder's output does not decode: {e}"),
        }
    }
});
