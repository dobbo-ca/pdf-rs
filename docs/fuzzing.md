# Fuzzing

`tests/fuzz` is a cargo-fuzz crate (`pdf-fuzz`, its own `Cargo.toml` and `Cargo.lock`, a
`[workspace]` of its own so only it depends on `libfuzzer-sys`). It has one target per parser
entry point (design.md section 13). pdf-go has no `Fuzz*` functions and no `testdata/fuzz`, so the
seeds come from the test corpus.

Do not build or run it on a laptop: push a branch and use CI.

```
gh workflow run fuzz.yml --ref <branch> -f max_total_time=60
```

## Targets

All run under `tests/fuzz/common.rs` `TIGHT` limits (a decoded stream of at most 4 MiB, 16 MiB of
decoded bytes and 128 MiB of working memory per document, one thread), so one input stays far under
`-malloc_limit_mb=512`. A panic, an allocation outside the limits, a hang or a failed assertion is
the finding. The document targets also check the ledger: the peak is at most `max_alloc`, and
dropping the document gives every charged byte back.

| Target | Entry point | Also checks |
|---|---|---|
| `read` | `api::read`, then up to 8 `page_dict` calls | ledger |
| `read_validate` | `api::read_and_validate` | ledger |
| `rvo` | `read_validate_and_optimize`; the last byte picks the command: validate, add properties (`properties_add`) or list properties (`properties`) | ledger |
| `write_fallback` | `read_validate_and_optimize`, `write_context` with `WriteEnv::parity()`, then a read of the output | the output starts with `%PDF-` |
| `extract_image` | a one-page PDF built from the first 8 bytes (size, bits per component, colour space, filter, mask and decode flags) and the rest as the image stream; `color_space_components`, `extract_image` | an image has bytes and a known file type |
| `filter_dct_cmyk` | the DCT filter, then `gob::decode_cmyk` of its output | limits |
| `filter_flate` | Flate with the predictors (the first four bytes pick `Predictor`, `Colors`, `BitsPerComponent`, `Columns`) | encode then decode gives the input back |
| `filter_lzw` | LZW (`Predictor`, `EarlyChange`, `Colors`, `BitsPerComponent`, `Columns`) | limits |
| `filter_ascii85`, `filter_asciihex`, `filter_runlength` | the filter on arbitrary bytes | limits |
| `filter_ccitt` | CCITT (`K`, `Columns`, `Rows`, `BlackIs1`, `EncodedByteAlign`) | limits |
| `png_encode` | the PNG writer on a generated gray or NRGBA image | read back with an independent decoder (deflate-rs inflate, CRCs, row unfiltering): the pixels equal the input |
| `gob` | `gob::decode_cmyk` on the input, then encode a generated image and decode it into a CMYK image | Pix, Stride and Rect come back |
| `gocap` | `gocap::read_from_cap` (the closed form) against `simulate_read_from` (the step-by-step `bytes.Buffer` simulation) for random read splits | equal; the write simulation keeps the length |
| `parse_object` | `parse::parse_object`, plain and with a `max_parsed_bytes` counter and depth 8 | the counter never passes its limit |
| `parse_content` | `parse::content::parse_content` (the first byte picks `relaxed` and `pdf20`) | |

The filter targets use the first bytes as decode parameters (a byte of 0 leaves the key out), so
the mutator reaches the predictor and CCITT parameter space without a PDF around it. A filter
error is expected on random bytes and is not a finding; `Error::Panic` stands for a site where Go
panics and is not one either.

`png_encode` needs the crate-private PNG writer, which `pdf::fuzz_api` (hidden from the docs,
not API) exposes. It and the gob target depend on `deflate` and `jpeg` at the same revisions as
the crate.

## Seeds and corpus

- `tests/fuzz/seeds/<target>/` holds a few small committed seeds per target (a minimal PDF, a
  filter stream with its parameter bytes, ...).
- The image build adds `tests/fuzz/seeds/pdf/` for the four document targets: the PDFs under
  300 KB of `tests/data`, and a sample (every 25th, at most 400, under 64 KB) of what
  `oracle -gen` writes (mutations, synthesized and encrypted PDFs). The `filter/*.txt` texts are
  added to the `parse_content` seeds.
- The corpus that a run grows is kept between runs: `fuzz.yml` restores it from the Actions cache
  (`fuzz-corpus-<target>-<run id>`, newest first), copies it into the container, and saves it back
  after the run. New inputs go to `tests/fuzz/corpus/<target>`.

## CI

`.github/workflows/fuzz.yml` runs nightly (`17 3 * * *`) and on `workflow_dispatch`, never on push
or pull request.

- A matrix over the 17 targets and two architectures (`x64` on `ubuntu-24.04`, `arm64` natively
  on `ubuntu-24.04-arm`) with `strategy.max-parallel: 2` (to bound runner use), ASan on both,
  `-max_total_time` from the `max_total_time` input (default 600 s), `-timeout=20`,
  `-rss_limit_mb=2048 -malloc_limit_mb=512`. One corpus cache per architecture and target.
- `docker create` then `docker start`, not a `RUN` layer, so a BuildKit cache hit cannot skip the
  fuzzing. Crash artifacts are copied out and uploaded as `crashers-<target>-<arch>`.
- The image builds from the stage `fuzz` of `.github/ci/Dockerfile`; the fuzz crate's
  dependencies are fetched in the `deps` stage with the other manifests, and the targets build
  offline.

To reproduce a crash, download the `crashers-<target>` artifact and run, in the `fuzz` image or on
a Linux machine with nightly and cargo-fuzz:

```
cargo +nightly fuzz run --fuzz-dir tests/fuzz <target> <crash file>
```

Fuzz hours for the status line are the sum of `max_total_time` times targets over the runs on
`main`.
