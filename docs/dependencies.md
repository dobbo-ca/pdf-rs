# Dependencies

The conventions put dependency reasons here, not in the README. `Cargo.lock` is committed.

| Crate | Source | Why |
|---|---|---|
| `deflate` | git, `dobbo-ca/deflate-rs`, rev `211232f` | inflate and zlib decoding with limits and cancellation. It only decodes; the encoder is in-crate (`src/filter/goflate`) because Go's output must be reproduced byte for byte |
| `jpeg` | git, `dobbo-ca/jpeg-rs`, rev `c0cf0fc` | the image types `src/filter/gojpeg.rs` returns (`Gray`, `YCbCr`, `Rgba`, `Cmyk`, `Rect`, `Subsample`), the error type behind `Error::Jpeg`, and the encoder the tests build JPEGs with. The decode itself is the in-crate port of Go's `image/jpeg` |
| `rayon` | crates.io, `=1.12.0` | the worker pool, justified in [threading.md](threading.md) |

Both git dependencies are public GitHub repositories, fetched once in the `deps` stage of
`.github/ci/Dockerfile`; every later step builds offline.
There is no `cc`, `bindgen` or `-sys` crate, and no FFI. The separate fuzz crate (`tests/fuzz`)
adds `libfuzzer-sys`, which the conventions allow there only.

The Go side of the parity harness has no private imports: pdf-go's non-test sources and the public
modules they build with are vendored under `tests/parity/go` ([parity.md](parity.md)).
