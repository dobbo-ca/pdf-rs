#![no_main]
use libfuzzer_sys::fuzz_target;
use pdf::Mem;
use pdf::filter::{DCT, Parms, new_filter};
use pdf::fuzz_api::gob_decode_cmyk as decode_cmyk;

#[path = "../common.rs"]
mod common;

// DCTDecode on arbitrary bytes. The filter returns the decoded image as the gob stream pdf-go
// puts in the stream's content; when it decodes, that gob stream is decoded into an `image.CMYK`
// as the CMYK renderer does. The seeds are real JPEGs cut from the DCT streams of tests/data
// (baseline gray with restart markers, baseline YCbCr 4:2:0, progressive, YCCK with and without
// restart markers, with the Adobe APP14 marker), and the hand-made `adobe-only`.
fuzz_target!(|data: &[u8]| {
    common::run_filter(DCT, &[], data);
    let Ok(f) = new_filter(DCT, Parms::new(), Some(common::decode_limit())) else {
        return;
    };
    let mem = Mem::new(&common::TIGHT);
    let cancel = common::never();
    match f.decode(data, &mem, &cancel) {
        Ok(d) => {
            if let Err(e) = decode_cmyk(d.as_slice()) {
                common::report(&e);
            }
        }
        Err(e) => common::report(&e),
    }
});
