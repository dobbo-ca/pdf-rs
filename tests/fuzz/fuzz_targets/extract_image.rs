#![no_main]
use libfuzzer_sys::fuzz_target;
use pdf::model::EntryObject;
use pdf::object::Object;

#[path = "../common.rs"]
mod common;

/// A one-page document with one image XObject. The first 8 bytes of the input pick the image
/// dictionary (size, bits per component, colour space, filter, flags); the rest is the stream.
fn image_pdf(data: &[u8]) -> Vec<u8> {
    let (head, body) = data.split_at(data.len().min(8));
    let b = |i: usize| head.get(i).copied().unwrap_or(0);
    let mut w = u32::from(b(0)) + 1;
    let h = u32::from(b(1)) + 1;
    if b(7) & 1 != 0 {
        // Up to 65 536 x 256: over the pixel limit now and then.
        w <<= 8;
    }
    let bpc = [1, 2, 4, 8, 16, 0, 3, 32][usize::from(b(2)) % 8];
    let cs = [
        "/DeviceGray",
        "/DeviceRGB",
        "/DeviceCMYK",
        "[/Indexed /DeviceRGB 1 <000000FFFFFF>]",
        "[/CalGray << /WhitePoint [0.95 1 1.09] >>]",
        "[/Lab << /WhitePoint [0.95 1 1.09] >>]",
        "[/Indexed /DeviceCMYK 1 <00000000FF000000>]",
        "/Bogus",
    ][usize::from(b(3)) % 8];
    let filter = match b(4) % 8 {
        1 => "/Filter /FlateDecode".to_string(),
        2 => "/Filter /LZWDecode".to_string(),
        3 => "/Filter /ASCIIHexDecode".to_string(),
        4 => "/Filter /ASCII85Decode".to_string(),
        5 => "/Filter /RunLengthDecode".to_string(),
        6 => format!("/Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns {w} /Rows {h} >>"),
        7 => "/Filter /DCTDecode".to_string(),
        _ => String::new(),
    };
    let mut extra = String::new();
    if b(5) & 1 != 0 {
        extra.push_str(" /ImageMask true");
    } else {
        extra.push_str(&format!(" /ColorSpace {cs}"));
    }
    if b(5) & 2 != 0 {
        extra.push_str(" /Decode [1 0]");
    }

    let mut f = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /Im0 4 0 R >> >> >>"
            .to_string(),
    ];
    for (i, o) in objects.iter().enumerate() {
        offsets.push(f.len());
        f.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    offsets.push(f.len());
    f.extend_from_slice(
        format!(
            "4 0 obj\n<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /BitsPerComponent {bpc}{extra} {filter} /Length {} >>\nstream\n",
            body.len()
        )
        .as_bytes(),
    );
    f.extend_from_slice(body);
    f.extend_from_slice(b"\nendstream\nendobj\n");
    let xref = f.len();
    let n = offsets.len() + 1;
    f.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
    for o in offsets {
        f.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    f.extend_from_slice(
        format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    f
}

// `color_space_components` and `extract_image` on an image whose dictionary comes from the first
// bytes of the input: every colour space, filter and bit depth the renderers know, with sizes
// that now and then pass `max_image_pixels`. A panic, an allocation outside the limits or a hang
// is the finding; a rendered image must have a file type and bytes.
fuzz_target!(|data: &[u8]| {
    let cancel = common::never();
    let pdf_bytes = image_pdf(data);
    let mut ctx = match pdf::api::read(&pdf_bytes, &common::config(pdf::Cmd::Validate), &cancel) {
        Ok(ctx) => ctx,
        Err(e) => return common::report(&e),
    };
    let numbers: Vec<i64> = ctx
        .xref
        .table
        .iter()
        .filter(|(_, e)| {
            matches!(&e.object, Some(EntryObject::Obj(Object::Stream(sd)))
                if sd.dict.subtype() == Some(b"Image".as_slice()))
        })
        .map(|(n, _)| *n)
        .take(4)
        .collect();
    for n in numbers {
        let Some(EntryObject::Obj(Object::Stream(sd))) =
            ctx.xref.table.get(&n).and_then(|e| e.object.as_ref())
        else {
            continue;
        };
        let mut sd = (**sd).clone();
        sd.content = None;
        if let Err(e) = pdf::api::color_space_components(&mut ctx.xref, &sd) {
            common::report(&e);
        }
        match pdf::api::extract_image(&mut ctx, &mut sd, b"Im0", n, &cancel) {
            Ok(Some(img)) => {
                // A passed-through jpg/jpx/jbig2 keeps the raw stream, which may be empty (as in Go).
                if ["png", "tif"].contains(&img.file_type) {
                    assert!(!img.data.is_empty(), "an empty image file");
                }
                assert!(
                    ["png", "tif", "jpg", "jpx", "jbig2"].contains(&img.file_type),
                    "file type {}",
                    img.file_type
                );
            }
            Ok(None) => {}
            Err(e) => common::report(&e),
        }
    }
    common::settle(ctx);
});
