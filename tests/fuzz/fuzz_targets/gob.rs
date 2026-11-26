#![no_main]
use jpeg::{Cmyk, Gray, Rect, Rgba};
use libfuzzer_sys::fuzz_target;
use pdf::filter::gojpeg::GoImage;
use pdf::fuzz_api::{gob_decode_cmyk as decode_cmyk, gob_encode_image as encode_image};

// The gob codec. The decoder first gets the raw input (it must not panic or allocate more than
// the input holds). Then the first nine bytes choose an image type (gray, RGBA or CMYK), a
// stride and a rectangle, the rest is its pixel array: the image is gob-encoded as pdf-go
// encodes a decoded JPEG and decoded into an `image.CMYK`, which must give the same pixels,
// stride and rectangle back (the three types share those field names).
fuzz_target!(|data: &[u8]| {
    let _ = decode_cmyk(data);
    if data.len() < 9 {
        return;
    }
    let (head, pix) = data.split_at(9);
    let stride = usize::from(head[1]) * 4;
    let (x0, y0) = (i32::from(head[2] as i8), i32::from(head[3] as i8));
    let (x1, y1) = (i32::from(head[4] as i8), i32::from(head[5] as i8));
    let rect = Rect::new(x0, y0, x1, y1);
    let pix = pix.to_vec();
    let img = match head[0] % 3 {
        0 => GoImage::Gray(Gray {
            pix: pix.clone(),
            stride,
            rect,
        }),
        1 => GoImage::Rgba(Rgba {
            pix: pix.clone(),
            stride,
            rect,
        }),
        _ => GoImage::Cmyk(Cmyk {
            pix: pix.clone(),
            stride,
            rect,
        }),
    };
    let msgs = match encode_image(&img) {
        Ok(m) => m,
        Err(e) => panic!("the encoder refused a small image: {e}"),
    };
    let mut bytes = Vec::new();
    for m in &msgs {
        bytes.extend_from_slice(&m.to_vec().expect("a message is encodable"));
    }
    let got = match decode_cmyk(&bytes) {
        Ok(got) => got,
        Err(e) => panic!("the decoder refused the encoder's output: {e}"),
    };
    assert_eq!(got.pix, &pix[..], "Pix");
    assert_eq!(got.stride, stride as i64, "Stride");
    let r = got.rect;
    assert_eq!(
        (r.min.x, r.min.y, r.max.x, r.max.y),
        (i64::from(x0), i64::from(y0), i64::from(x1), i64::from(y1)),
        "Rect"
    );
});
