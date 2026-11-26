# Benchmarks

The target is at most 1.10 times Go's time on every class, with Rust at the default `max_threads`
against Go at its default, and both single-threaded (`max_threads = 1` against `GOMAXPROCS=1`).
There is no benchmark framework: `examples/bench.rs` (Rust) and `oracle -bench` (Go,
`tests/parity/go/oracle/bench.go`) time the same operation on the same file with `Instant` and
`time.Now`, and `tests/parity/bench.sh` joins the two outputs into tables.

## Running

`.github/workflows/bench.yml` (`workflow_dispatch` and push to `main`, not a required check) builds
the `bench` stage of `.github/ci/Dockerfile` and runs `tests/parity/bench.sh` in it with
`docker run`, so Go and Rust run on the same machine and a BuildKit cache hit cannot skip the
timing. The tables are in the job summary. Locally the pieces are:

```
oracle -genbench tests/data /bench-in      # the inputs, named <class>__<file>
oracle -bench 5 /bench-in                  # Go: name TAB bytes TAB ns per run
bench /bench-in 5 [threads]                # Rust: the same lines
sh tests/parity/bench.sh [N]               # both, best of three, two tables
```

`bench.sh` takes the best of three runs per file and side (the machine is shared). It exits 1 when
a class is over 1.10, when a byte count differs between Go and Rust, or when only one side timed a
file. A single file over 1.10 is marked but does not fail the run by itself. A file whose
operation fails is skipped, with a line on stderr.

## Classes

The class is the prefix of the input's name. The setup of an operation (the read that
`decode-flate` and the image classes start from) is not timed. Go does not run the determinism
hooks in this mode, and Rust uses `WriteEnv::default()`.

| Class | Inputs | Operation | Bytes counted |
|---|---|---|---|
| `read-small` | test.pdf, empty.pdf, testRot.pdf | `read` | input |
| `read-objstm` | CenterOfWhy.pdf, adobe_supplement_iso32000_1.pdf | `read` | input |
| `read-large` | WaldenFull.pdf, FOSDEM14_HPC_devroom_14_GoCUDA.pdf, gobook.0.pdf | `read` | input |
| `validate` | the eight files above | `api::validate` | input |
| `rvo` | the eight files above | `read_validate_and_optimize` | input |
| `write-fallback` | CenterOfWhy.pdf encrypted with AES-256 and with RC4-128 (made by `oracle -genbench`) | `read`, then `write_context` | input |
| `decode-flate` | WaldenFull.pdf, gobook.0.pdf | `decode_with_limit` of every stream | decoded |
| `encode-flate` | filter/Mark.Twain-Tom.Sawyer.txt, pi.txt, e.txt | `new_stream_dict_for_buf` and `encode` | encoded |
| `image-png` | testImage.pdf, a synthetic gray and a synthetic RGB image | `extract_image` of every image (up to 64) | image files |
| `image-tiff` | a synthetic CMYK image | `extract_image` | image files |
| `image-dct-gray` | a synthetic 1024x1024 gray JPEG (made by `oracle -genbench` with Go's `image/jpeg`) | `extract_image` | image files |
| `image-dct-rgb` | a synthetic 1024x1024 YCbCr JPEG (same encoder) | `extract_image` | image files |
| `image-dct-cmyk` | the first two CMYK DCT images found in the corpus, each in a document of its own, and a synthetic 1024x1024 baseline CMYK JPEG (`oracle -genbench` writes it itself: `image/jpeg` cannot encode CMYK) | `extract_image` | image files |
| `image-ccitt` | T6.pdf, HL1396.pdf | `extract_image` | image files |

The `image-dct-cmyk` class is empty if the corpus has no CMYK DCT image (`-genbench` says so on
stderr). The two corpus images are small (232x232 and 150x193, baseline), so the synthetic 1024x1024
one is what measures the decoder. The gray and RGB classes use the same operation on large inputs;
the extractor may pass those JPEGs through without decoding them, in which case they time that path
and not the decoder.

## Results

### Hosted baseline

From the `bench` job, run 45729761728 on a GitHub-hosted `ubuntu-24.04` runner (2 vCPU), the
baseline for later hosted runs. Shared VMs are noisy; these numbers are not comparable with the
dedicated-machine numbers below. Rust default `max_threads` against Go default, per class:

| class | files | MB | Go ms/MB | Rust ms/MB | Rust / Go |
|---|---|---|---|---|---|
| read-small | 3 | 0.00 | 72.770 | 26.636 | 0.37 |
| read-objstm | 2 | 2.69 | 3.413 | 1.582 | 0.46 |
| read-large | 3 | 9.76 | 1.930 | 0.830 | 0.43 |
| validate | 8 | 12.45 | 19.286 | 10.045 | 0.52 |
| rvo | 8 | 12.45 | 19.357 | 10.065 | 0.52 |
| write-fallback | 2 | 4.74 | 16.871 | 13.886 | 0.82 |
| decode-flate | 2 | 46.46 | 2.967 | 0.686 | 0.23 |
| encode-flate | 3 | 0.26 | 31.774 | 24.370 | 0.77 |
| image-png | 3 | 0.54 | 194.145 | 111.908 | 0.58 |
| image-tiff | 1 | 1.05 | 9.583 | 3.017 | 0.31 |
| image-dct-gray | 1 | 0.47 | 0.187 | 0.022 | 0.12 |
| image-dct-rgb | 1 | 0.63 | 0.271 | 0.028 | 0.10 |
| image-dct-cmyk | 3 | 1.92 | 83.872 | 58.788 | 0.70 |
| image-ccitt | 2 | 6.78 | 832.709 | 454.095 | 0.55 |
| all | 42 | 100.21 | 66.416 | 36.135 | 0.54 |

No class and no file is over 1.10 (the highest file is `write-fallback__aes-256.pdf` at 1.06).

### Earlier result (dedicated machine)

From the `bench` job, run 44211418836 on the branch head e4fd8cb (AMD Ryzen 7 9800X3D, 8 cores, the Go
oracle and the Rust bench example in one image; best of three runs per file). Ratio is Rust time over
Go time; every class is below 1.10 in both tables (15 classes, 42 files).

### Rust default max_threads against Go default

| class | files | MB | Go ms/MB | Rust ms/MB | Rust / Go |
|---|---|---|---|---|---|
| read-small | 3 | 0.00 | 62.652 | 17.575 | 0.28 |
| read-objstm | 2 | 2.69 | 1.688 | 1.077 | 0.64 |
| read-large | 3 | 9.76 | 1.004 | 0.552 | 0.55 |
| validate | 8 | 12.45 | 11.533 | 6.614 | 0.57 |
| rvo | 8 | 12.45 | 11.426 | 6.611 | 0.58 |
| write-fallback | 2 | 4.74 | 9.643 | 8.770 | 0.91 |
| decode-flate | 2 | 46.46 | 1.765 | 0.484 | 0.27 |
| encode-flate | 3 | 0.26 | 21.341 | 16.974 | 0.80 |
| image-png | 3 | 0.54 | 123.777 | 75.918 | 0.61 |
| image-tiff | 1 | 1.05 | 5.858 | 1.816 | 0.31 |
| image-dct-gray | 1 | 0.47 | 0.086 | 0.014 | 0.17 |
| image-dct-rgb | 1 | 0.63 | 0.069 | 0.014 | 0.21 |
| image-dct-cmyk | 3 | 1.92 | 54.830 | 41.489 | 0.76 |
| image-ccitt | 2 | 6.78 | 537.429 | 288.071 | 0.54 |
| all | 42 | 100.21 | 42.457 | 23.117 | 0.54 |

### Rust max_threads 1 against Go GOMAXPROCS=1

| class | files | MB | Go ms/MB | Rust ms/MB | Rust / Go |
|---|---|---|---|---|---|
| read-small | 3 | 0.00 | 31.711 | 17.315 | 0.55 |
| read-objstm | 2 | 2.69 | 1.788 | 1.077 | 0.60 |
| read-large | 3 | 9.76 | 1.086 | 0.551 | 0.51 |
| validate | 8 | 12.45 | 13.612 | 6.568 | 0.48 |
| rvo | 8 | 12.45 | 13.538 | 6.585 | 0.49 |
| write-fallback | 2 | 4.74 | 10.735 | 8.770 | 0.82 |
| decode-flate | 2 | 46.46 | 1.745 | 0.483 | 0.28 |
| encode-flate | 3 | 0.26 | 20.298 | 16.941 | 0.83 |
| image-png | 3 | 0.54 | 120.538 | 75.957 | 0.63 |
| image-tiff | 1 | 1.05 | 5.250 | 1.824 | 0.35 |
| image-dct-gray | 1 | 0.47 | 0.057 | 0.014 | 0.24 |
| image-dct-rgb | 1 | 0.63 | 0.145 | 0.014 | 0.10 |
| image-dct-cmyk | 3 | 1.92 | 53.163 | 41.509 | 0.78 |
| image-ccitt | 2 | 6.78 | 542.419 | 287.823 | 0.53 |
| all | 42 | 100.21 | 43.309 | 23.091 | 0.53 |

One file is over 1.10 at default threads: `write-fallback__aes-256.pdf` at 1.12 (Go 8.770 ms/MB, Rust
9.832 ms/MB); its class is 0.91, and the same file is 1.00 single-threaded (Go 9.819 ms/MB, Rust
9.836 ms/MB). The file is AES-256
encrypted, so the time is the cipher: Rust's AES is the portable table code (a port of Go's
`aes_generic.go`) and Go's AES uses AES-NI. The crate forbids `unsafe` by default, but
the port conventions allow an `#[allow(unsafe_code)]` kernel module for a Go assembly
fast path, so an AES-NI kernel is possible and is not ported. Go's
single-threaded figure for the file is 9.819 ms/MB against 8.770 at default threads, so the 1.12 comes
from Go's default-threads run being faster, not from Rust being slower. `bench.sh` marks a file over
1.10 without failing the run, as here.

If the object-stream or validate classes are above 1.10, threading phase 2 (WP 29,
[threading.md](threading.md)) is the planned remedy; anything else above 1.10 is a port of Go's
fast path before the crate is done.
