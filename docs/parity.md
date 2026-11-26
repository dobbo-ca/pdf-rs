# Parity with pdf-go

The parity job runs pdf-go (a vendored snapshot at 113d693, plus a small determinism patch) and
pdf-rs on the same inputs and requires the output trees to be byte-equal. The operations are
`read`, `pages`, `streams`, `budget` and `encode` (WP 10), `images`, `validate`, `rav`, `rvo`, `propsadd`,
`props`, `write` and `writeopt` (WP 27), and the input-independent `build` (docs/design.md
section 12). 13 operations run on every input under every configuration, and `build` runs once.
The `filter/*.txt` inputs run only `encode` and `zlevel` (the goflate encoder at levels 1 and 9).

```
tests/parity/go/upstream/   pdf-go non-test sources, LICENSE.txt, go.mod (trimmed), verbatim
tests/parity/go/patches/    determinism.patch, applied to a copy of upstream at build time
tests/parity/go/deps/       the public Go modules the build needs, each with its licence
tests/parity/go/oracle/     main.go (read side, classify), ops.go (the WP 27 operations and
                            `build`), gen.go (-gen), go.mod (replace directives)
tests/parity/go/build.sh    copies upstream, deps and oracle, applies the patch, builds
tests/parity/diff.sh        diff -rq of two trees, then the differing sections counted by kind, with two examples each (about 400 lines in all)
examples/oracle.rs          the Rust side
tests/data/                 the corpus
.github/ci/Dockerfile       stages go-oracle, go-run, parity
```

## Running it

```
GOFLAGS=-mod=mod GOPROXY=off sh tests/parity/go/build.sh /tmp/oracle
/tmp/oracle -gen tests/data /inputs                     # the corpus and its mutations
/tmp/oracle /inputs /go-out
/tmp/oracle /inputs /go-out-2; sh tests/parity/diff.sh /go-out /go-out-2    # the run-twice check
cargo run --release --example oracle -- -threads 1 /inputs /rust-out-1
cargo run --release --example oracle -- /inputs /rust-out
sh tests/parity/diff.sh /go-out /rust-out-1; sh tests/parity/diff.sh /go-out /rust-out
```

CI does the same in the `parity` Docker stage (`target: parity` of `.github/ci/Dockerfile`, on
`main` and on `workflow_dispatch`). The Go oracle runs with `GOMEMLIMIT=3GiB`, twice (the
`go-run` stage fails if the two trees differ, see Determinism). The Rust oracle runs twice, with
`-threads 1` and with the default `max_threads`; both outputs must equal Go's. Each Rust tree is deleted after its diff, pass or fail, and the stage checks
that `/go-out` has at least twice as many files as `/inputs` (about inputs x configs); `go-run`
prints `du -sh` and the file count of the Go tree.

The `parity-arm64` stage (`target: parity-arm64`, scope `pdf-rs-parity-arm64`, on `main` and
`workflow_dispatch`) builds the oracle example natively on `ubuntu-24.04-arm` and runs it on every
16th input and its `.limits` sidecar (`go-run` runs Go on the same subset into `/go-sub`): once with
`-threads 1`, once with `DEFLATE_SIMD=scalar JPEG_SIMD=scalar`; both outputs must equal Go's. The two variables are
read by the deflate-rs and jpeg-rs dependencies, which have SIMD tiers; pdf-rs has none, and
Go's `image/jpeg` has none.

## Vendoring

Nothing is fetched at build time and no token is needed.

- `upstream/` holds the non-test `.go` files of the packages the oracle imports (`go list -deps`
  over `pkg/api`, `pkg/filter` and `pkg/pdfcpu/...`: `pkg/api`, `pkg/filter`, `pkg/font`,
  `pkg/log`, `pkg/pdfcpu/**` and `internal/{contextutil,corefont,fileutil,filter/lzw,inputlimit,
  netutil,streamlimit}`), the `go:embed` files of `pkg/pdfcpu/model/resources`, `LICENSE.txt`, the licence files of
  `internal/filter/lzw` (BSD, The Go Authors) and `pkg/pdfcpu/pkcs7` (MIT), and `go.mod`. Not copied: `cmd`, `pkg/cli`, `pkg/samples`, `pkg/testdata`, `_scripts`, `resources`
  and every `_test.go`. pdf-go has no `NOTICE` file. `go.mod` keeps the `go` line and the
  requirements the build uses, with their versions; the requirements of `cobra` and
  `jsonschema-go`, which only `cmd` and `pkg/cli` import, are dropped.
- `deps/<name>/` holds, for each module in the build, the non-test `.go` files of the imported
  packages (whole package directories, so build tags work), at the version of pdf-go's `go.mod`
  and `go.sum` (the copies are from the module cache), and the licence files (`LICENSE`,
  `PATENTS`). Each `go.mod` is trimmed to `module` and `go`: Go loads the `go.mod` of every module
  of an unpruned graph, and the dependencies of these modules (for example `x/sys`, `x/tools`) are
  not vendored.
- The oracle's `go.mod` requires every module and `replace`s it to its directory. Directory
  replacements need no `go.sum`.

Nothing in the vendored tree is edited but the `go.mod` files (trimmed, above). `build.sh` copies `upstream/`, `deps/` and `oracle/` to a
temporary directory, applies `patches/determinism.patch` there with `git apply -p1`, and builds
there. The oracle does not build against the plain snapshot, because it sets the two hooks
(`pdfcpu.TimeNow`, `pdfcpu.RandReader`) that the patch adds.

## Inputs

`oracle -gen <data dir> <input dir>` writes, flattening `a/b.pdf` to `a__b.pdf`:

1. Every PDF of `tests/data` unchanged, and `filter/*.txt` (the encode op's inputs, from
   `pkg/filter/testdata`). `goflate/` is not a PDF corpus and is skipped.
2. Mutations of each PDF, seeded by the file name (FNV-1a), with the tier by size: up to 300 KiB
   everything below; up to 3 MiB the generic ones with 6 bit flips and the `startxref`, xref
   offset and `/Size` rules; above that four truncations. A variant whose pattern is absent is
   not written.
   - **Generic:** truncations (`t0`..`t7`: n/8, n/4, n/2, 3n/4, n-1, inside the last xref, inside
     the first stream, n/16), 16 bit flips (`f`), deletions of `stream`, `endstream` and `endobj`
     and of a byte next to them (`d`).
   - **Structural rules** (`gen.go`, table `rules`): `startxref` ±1, 0, huge (`sx`); an xref entry
     offset (`xo`); `/Size` -1, 2^31, 0, huge (`sz`); `/Length` ±1, 0, huge, indirect to the
     free object 0 (`ln`); `/Filter` swaps to unknown, JBIG2 mid-pipeline, Crypt first, JBIG2
     alone, LZW, Flate twice (`fl`); the DecodeParms grid: `/Predictor` {1, 2, 10-16, 99}
     (`pr`), `/Colors` {0, -1, 2^20} (`co`), `/BitsPerComponent` {0, 3, 16, 32} (`bp`),
     `/Columns` {0, -1, 2^30} (`cl`), `/EarlyChange` 0 (`ec`), `/K` {1, -1} (`kk`), `/Rows` 0
     (`rw`), `/BlackIs1` (`b1`), `/EncodedByteAlign` (`ba`); object stream `/N` and `/First`
     (`on`, `of`); xref stream `/W` (`xw`); root removed (`rt`, `ct`); image edits: `/Width`,
     `/Height`, `/ColorSpace`, `/Decode`, `/SMask` pointing to a non-stream, Indexed hival {0,
     255, 256}, ICCBased `/N` {2, 5} (`iw`, `ih`, `ics`, `idc`, `ism`, `ihv`, `icn`, `ica`);
     four bit flips inside the first stream body only (`sf`).
3. Synthesized PDFs `synth.*`, made with pdf-go's own encoders:
   - a filter {Flate, LZW, ASCII85, ASCIIHex, RunLength} x payload {text, zeros, random} x
     {valid, truncated to 60%, three corrupted bytes} matrix;
   - two pipelines (`[/ASCII85Decode /FlateDecode]`, Flate twice);
   - a Flate DecodeParms grid, encoded with valid parameters and read with the deviation
     (`synth.parms.*`);
   - LZW with `/EarlyChange 0` and with `/Predictor 2`;
   - Flate streams with sync-flush markers every 1, 1000, 32768, 40000 and 100000 bytes, for the
     capacity model (`synth.sync.*`);
   - arrays and dictionaries nested 50, 99, 100, 101, 102 and 200 deep (`synth.nest.*`);
   - a document with its pages in an object stream and an xref stream, with `/N` 3, 0, 99 and
     999999999 (`synth.objstm.*`).
   - PDF 2.0 documents with annotations and core fonts, for the write path (`synth.pdf20.*`,
     `pdfDoc` in `gen.go`): Helvetica alone, the 14 families, `/WinAnsiEncoding` and
     `/MacRomanEncoding`, an encoding with `/Differences`, Symbol and ZapfDingbats with
     `/WinAnsiEncoding` (no metrics for most glyphs: an error in both), a font with its own
     `/Widths`, subset and Arial names, two dictionaries that are not core fonts, a 1.7 header
     with `/Version /2.0` in the catalog, and an information dictionary.
   - Not yet: CCITT, DCT and image matrices (WP 11, 12).
4. Encrypted variants of `testWithText.pdf` and `test.pdf` made with the snapshot's `api.Encrypt`:
   RC4-40, RC4-128, AES-128 and AES-256 with an empty user password, with and without object
   and xref streams (`enc.<alg>[.stm]`); a non-empty user password for RC4-40 and AES-256
   (`.pw`, the read answers `wrong-password`); and single-byte edits of `/O`, `/U`, `/OE`,
   `/UE`, `/Perms` and `/P` in the plain-stream variants. Encryption uses random salts, so these
   files differ between runs; both oracles read the same files.
5. Boundary variants of every input up to 64 KiB with a readable stream: a copy `<n>.bd0`..`bd2`
   with `max_decode_bytes` at (longest decoded stream) -1, 0, +1, and `<n>.bt0`, `bt1` with
   `max_total_decode_bytes` at (sum of the Go capacities of the decoded streams) -1 and 0. The
   sidecar `<input>.limits` holds `key=value` lines named after the fields of the Rust `Limits`
   (`max_decode_bytes=1234`); an input with a sidecar also runs the configuration `b`.

## Configurations

| | |
|---|---|
| `d` | byblos's defaults (`pdfdoc.go:81-93`): decode 64 MiB, total decode 64 MiB, object bytes 64 MiB, object count 200 000, object stream entries 200 000, parsed bytes 297 508 864 (`ObjectTableBytes(200 000)/4*3 + 64 MiB`); the rest `DefaultResourceLimits()` |
| `g` | `DefaultResourceLimits()` |
| `s` | decode 64 KiB, total decode 256 KiB, parsed 1 MiB, object stream entries 1000, stream total 1 MiB, image pixels 1 M, object bytes 1 MiB |
| `b` | `g` with the sidecar's values |

Both sides read with `model.NewStatelessConfiguration()` (Rust: `Config::default()`) and the
command `VALIDATE`.

## Operations and files

For input `<n>` and configuration `<c>` there is ONE file, `<n>.<c>.txt` (one file per op made
1.6 million files for 8 138 inputs, which filled the BuildKit overlayfs). It holds one
section per result, in the order of `op` then `kind` (bytewise), each a header line
`== <op> <kind>` followed by the content. The "Files" column below lists each section as
`.<op>.<kind>`: `.read.dump` is the section `== read dump`, `.s7.cap` is `== s7 cap`. Whole-operation
errors are the section `== <op> err`, holding the class token and a newline. Raw bytes are never
written: `write.out` (and `_build.g` `write out`) is `<len>:<sha256>` and a newline, as `hs`
elsewhere, the same on both sides. The `-gen` inputs are unchanged. If `read` fails, no other
operation runs. Every Go call runs under `recover()`; a recovered panic is the class `panic`,
which Rust returns as `Error::Panic` at the same site. The Rust oracle does not catch Rust
panics: one fails the job.

| Op | Sections | Content |
|---|---|---|
| `read` | `.read.dump` | one `key value` line each for `size`, `root`, `info`, `id` (PDFString of the array), `encrypt`, `enc p= v= r=`, `header`, `rootversion`, `pagecount`, `maxobjnr`, `objstm`; then per object number ascending `obj N gen G free F ` and the body: `nil` (also the null object), the PDFString, `stream <dict> raw <len>:<sha256> content <len>:<sha256>\|nil`, `objstm <dict> raw .. n .. first ..` (no content), `xrefstm <dict> raw .. size .. w a b c`, or `lazy <len>:<sha256>` of the object's text (`lazy err <class>`), which decodes the object stream, as `GetData` does |
| `pages` | `.pages.out` | `count N` (or `count err <class>`), then for pages 1..min(N, 256) `page P ref N G dict <PDFString> res <PDFString\|nil> mb <x y x y\|nil> cb <..> rot R`, or `page P err <class>`. Floats are the shortest round-trip decimal without exponent |
| `streams` | `.s<obj>.out`, `.s<obj>.cap` | for up to 512 plain streams, ascending, on a copy with `Content` reset, `DecodeWithLimit(decode limit)`: `ok <len>:<sha256>` or `err <class>`; and, on success, `cap(Content)` |
| `budget` | `.budget.out` | on a second, untouched read: `XRefTable.DecodeStream` over all plain streams ascending, one line `N ok <cap>` or `N <class>` each |
| `encode` | `.e<obj>.out` | for each stream that `streams` decoded (up to 4 MiB): `NewStreamDictForBuf` and `Encode`: `raw <len>:<sha256> length <n>`, or `err <class>`. For `filter__*.txt`: `<n>.g.e0.out`, once |
| `zlevel` | `.zlevel.1`, `.zlevel.9` | for `filter__*.txt` only, in `<n>.g.txt`: `zlib.NewWriterLevel` at `BestSpeed` (1) and `BestCompression` (9), one `Write` of the whole file, `Close`: `<len>:<sha256>` of the zlib stream, or `err <class>` (Rust: `ZlibWriter::with_level`) |
| `images` | `.images.out`, `.i<obj>.<type>.out` | on a read of its own, for up to 64 plain streams with `/Subtype /Image`, ascending, on a deep copy: `image N comps <k\|err class> extract <type> <len>:<sha256>\|err class\|nil cap <cap(Content)>`. `<type>` is the file type of the rendered image (`png`, `tif`, `jpg`, `jpx`, `jbig2`); each rendered file also has `.i<obj>.<type>.out` with its `<len>:<sha256>`. Go: `ColorSpaceComponents`, `ExtractImage(thumb=false, stub=false)`, `io.ReadAll` of the reader (`nil`: a nil image, a nil reader or an empty type) |
| `validate` | `.validate.ok` or `.validate.err` | `api.Validate` with `NewStatelessConfiguration`, command `VALIDATE` (it also optimizes, as `conf.Optimize` is set). `ok` holds `ok` and a newline |
| `rav` | `.rav.dump` | `ReadAndValidate` (`ReadContext` and `ValidateContext`, the `api.ReadContextFile` path, without optimize), then the `read` dump |
| `rvo` | `.rvo.dump` | `ReadValidateAndOptimize` (VALIDATE), then the `read` dump, then `names <sorted keys of XRefTable.Names, comma-separated>` and `form <true\|false>` (whether `XRefTable.Form` is set) |
| `propsadd` | `.propsadd.dump` | `ReadValidateAndOptimize` (ADDPROPERTIES), `PropertiesAdd({"Title": "parity", "Key": "Wert ü"})`, then the `read` dump |
| `props` | `.props.out` | `api.Properties`: sorted `key=value` lines, with `\`, newline and carriage return escaped as `\\`, `\n`, `\r` |
| `write` | `.write.out`, `.write.redump` | `ReadContext` then `api.WriteContext` into memory: the length and hash of the exact bytes; then the `read` dump of a read of those bytes under the same configuration (`err <class>` in the file if that read fails). An error of the write is `.write.err` |
| `writeopt` | `.writeopt.out`, `.writeopt.redump` | `ReadValidateAndOptimize` (VALIDATE) then `WriteContext`, then as `write`: the length and hash of the exact bytes (raw bytes would only print "Binary files differ"), and the `read` dump of a read of those bytes (`err <class>` if that read fails). An error of the write is `.writeopt.err` |
| `build` | file `_build.g.txt`: sections `build out`, `write out`, `text out` | once, with no input (see below) |

Only plain streams (`types.StreamDict`) are in `streams`, `budget`, `encode` and `images`; object
streams and xref streams are in the `read` dump. The decoded content is a length and a hash, not
the bytes, so the output stays small. Every operation after `budget` starts its own read of the
input bytes (`images`, `write`) or its own `api` call, so none sees the state another left. The
write operations run under `WriteEnv::parity()` on the Rust side and the patched hooks on the Go
side (see Determinism).

**`build`** has no input. It makes a document the way byblos does and writes: `create ver <v> size
<n> root <r>` (`CreateXRefTableWithRootDict`, `VersionString`); `inserted`, `rect`, `catalog`,
`pages`, `entry <n> found <b> [gen free refcount]` for objects 0, 1, 3, the inserted one and 99,
`entry page`, `mediabox`, `page`, `pagesdict`, `content <dict> raw <len>:<sha256> length <n>`
(`IndRefForNewObject`, `InsertObject`, `NewStreamDictForBuf`, `Encode`, `Catalog`, `Pages`,
`FindTableEntry`, `FindTableEntryForIndRef`, `Dereference`, `DereferenceArray`, `DereferenceDict`,
`DereferenceStreamDict`, `RectForDim`), and, after `CreateContext`, `write <len>:<sha256>` of the
`WriteContext` output (`write out` of `_build.g.txt` holds its hash). The section `text out` of `_build.g.txt` has the text
helpers on fixed inputs, with every text as hex: `date` (`DateTime(s, relaxed)` and `DateString`,
or `bad`), `lit` (`StringLiteralToString`), `hex` (`HexLiteralToString`) and `escape`
(`EscapedUTF16String`), each `ok <hex>` or `err <class>`.

## Results

Latest run: the PR head on main d474853, parity run 44212828011.

| | amd64 | arm64 subset (every 16th input) |
|---|---|---|
| inputs | 8,157 | 514 |
| output files (`<input>.<config>.txt`) | 24,024 | 1,507 |
| files that differ | 0 | 0 |

The amd64 diff is empty with `-threads 1` and with the default `max_threads`. The arm64 diff is
empty on the NEON tier and on the scalar tier of the dependencies (`DEFLATE_SIMD=scalar JPEG_SIMD=scalar`).

Per input: 13 operations plus `build` once, under the configurations `d`, `g`, `s` and, for inputs
with a `.limits` sidecar, `b`. The diffs are listed by `tests/parity/diff.sh`.

Test counts (grep, per the conventions' "count the Go test cases and the ported/generated cases"):

| | Count |
|---|---|
| Go test functions (`func Test`) in the in-scope packages | 906 |
| of which `pkg/pdfcpu` root (files of the in-scope sources: read, crypto, optimize, write, extract, image, info, property) | 296 |
| `pkg/pdfcpu/model` | 305 |
| `pkg/pdfcpu/types` | 30 |
| `pkg/pdfcpu/validate` | 110 |
| `pkg/filter` | 38 |
| `internal/filter/lzw` | 4 |
| `pkg/api` (`api_error_handling_test.go`, `property_error_handling_test.go`) | 123 |
| Rust `#[test]` functions | 963 (957 in `src/`, 6 in `tests/`) |
| `-gen` inputs | 8,157: 121 originals (117 PDFs, 4 `filter/*.txt`), 8,036 generated (mutations, `synth.*`, encrypted and boundary variants) |

The Go counts include whole test files of the in-scope packages, not only the tests of in-scope
functions; whole-package totals are 1,714. The split of the 8,036 generated inputs by rule is not
counted.

## Error classes

`classify` in `main.go` is the Go side of `Error::class()` (`src/error.rs`, docs/design.md
section 5), from the most specific case down. The Go test is `errors.Is`/`errors.As` where the
error is typed and a message match where it is not.

| Class | Go test |
|---|---|
| `panic` | a recovered panic |
| `wrong-password` | `pdfcpu.ErrWrongPassword` |
| `limit/object-bytes`, `limit/parsed`, `limit/stream-total` | `model.ErrObjectBytesExceeded`, `ErrParsedLimitExceeded`, `ErrStreamTotalLimitExceeded` |
| `limit/total-decode` | `ErrLimitExceeded` and `filter.ErrDecodeLimitExceeded` |
| `limit/objstm-entries` | `ErrLimitExceeded` with "object stream entries" in the message |
| `limit/objects` | any other `ErrLimitExceeded` |
| `limit/decode` | `filter.ErrDecodeLimitExceeded` |
| `limit/xref-range` | message "xref subsection object range exceeds limit" |
| `limit/image-pixels`, `limit/image-bytes` | the untyped messages of `validatePDFImageDimensions` (writeImage.go:132, :145): "pixel count .. exceeds limit" and "byte size exceeds limit" |
| `no-content`, `unsupported-filter`, `invalid-filter`, `unsupported-resource` | `model.ErrNoContent`, `filter.ErrUnsupportedFilter`, message "invalid filter: <", `pdfcpu.ErrUnsupportedResource` |
| `encryption/unsupported`, `encryption/malformed` | `pdfcpu.ErrUnsupportedEncryptionFeature`, `ErrMalformedEncryption` |
| `flate/corrupt`, `zlib/header`, `flate/checksum`, `zlib/dictionary` | `flate.CorruptInputError`, `zlib.ErrHeader`, `ErrChecksum`, `ErrDictionary` |
| `unexpected-eof` | `io.ErrUnexpectedEOF` |
| `lzw/invalid-code`, `ascii85/corrupt`, `ascii85/eod`, `hex/invalid-byte` | message "lzw: invalid code", `ascii85.CorruptInputError`, "ASCII85 decode: missing eod marker", `hex.InvalidByteError` |
| `jpeg/format`, `jpeg/unsupported` | `jpeg.FormatError`, `jpeg.UnsupportedError` |
| `ccitt`, `gob` | message contains "CCITT decode" or "ccitt:", "gob:" |
| `predictor/rows` | "flate decode: unexpected PNG predictor", "postprocessing failed", "read error, expected" |
| `predictor/params` | "undefined \"Predictor\"", "\"Colors\" must be", "\"Columns\" must be", "unexpected \"BitsPerComponent\"", "flate encode: TIFF predictor", "LZW decode: unsupported predictor" |
| `cancelled` | `context.Canceled` |
| `syntax` | an error that is one of the Go sentinels the Rust crate keeps as `Sentinel` variants (`errors.Is` against `model.Err*Cycle`, `Err*Duplicate`, `ErrMaxRecursionDepthExceeded`, `ErrPageNotFound`, `ErrExpectedDict`, `ErrMissingEncryptDictObject`, `ErrWrongTypeEncryptDictObject`, `ErrCorruptObjectOffset`, `ErrXRefStreamIndexSizeMismatch`, `pdfcpu.ErrReferenceDoesNotExist`, `validate.ErrMissingFont`, `validate.ErrBookmarksRepair`, and the messages "corrupt page content", "corrupt TJ expression", "corrupt BI expression"), even when validation wraps it |
| `validate` | the message contains "validation error": the prefix of `api.validationError`, put on every error of `ValidateContext`, so what is left is an untyped error of the validation pass (Rust: a `Validation`, `Corrupt` or unlisted `Sentinel` under the "validation error" context, whatever the variant) |
| `syntax` | everything else: the untyped `fmt.Errorf`/`errors.New` errors, which Rust has as `Corrupt`, `Validation` or an unlisted `Sentinel` outside that context (optimize and write errors are of this kind) |

Go-vs-Rust, for the limit errors a caller can tell apart (`Error::class()`, `src/error.rs`):

| Limit | Go | Rust | Class |
|---|---|---|---|
| `max_object_count` | `/Size` or subsection range error | `XRefObjectRange` or `Corrupt` | `limit/xref-range` or `syntax` |
| `max_xref_entries`, `max_object_stream_count`, `_first`, `max_stream_bytes` | untyped `fmt.Errorf` | `Corrupt` | `syntax` |
| `max_object_stream_total_count` | `ErrLimitExceeded` | `Limit(ObjectStreamEntries)` | `limit/objstm-entries` |
| `max_parsed_bytes`, `max_stream_total_bytes`, `max_object_bytes` | `ErrParsedLimitExceeded`, `ErrStreamTotalLimitExceeded`, `ErrObjectBytesExceeded` | `Limit(Parsed)`, `Limit(StreamTotal)`, `Limit(ObjectBytes)` | `limit/parsed`, `limit/stream-total`, `limit/object-bytes` |
| `max_total_decode_bytes` | `ErrLimitExceeded` + `ErrDecodeLimitExceeded` | `Limit(DecodeBudget)` | `limit/total-decode` |
| `max_decode_bytes` | `ErrDecodeLimitExceeded` | `DecodeLimit` | `limit/decode` |
| `max_image_pixels`, `max_image_bytes` | untyped message | `ImageLimit` | `limit/image-pixels`, `limit/image-bytes` |
| `max_alloc` | none | `Alloc` | `limit/alloc` |

Go never answers `limit/alloc`, `oom`, `flate/internal`, `flate/invalid-argument` or `io` (Rust
only). Because the fallback is `syntax`, a Go error that has its own Rust class but is not in the
table shows up as a diff, not as `other`. The `validate` rule goes by the message, so an error
that Rust makes a listed `Sentinel` where Go has an untyped error, or the other way round, is a diff.

## Determinism

Go randomises map iteration, so some of pdf-go's output depends on the run. Go leaves the order
unspecified, so sorted order is one allowed execution of the same program, and the Rust crate
iterates every map in sorted order (docs/design.md section 10). `patches/determinism.patch` makes
the Go snapshot do the same, in 31 hunks of 8 files (paths under `pkg/pdfcpu/`). A sorted loop
ranges over `slices.Sorted(maps.Keys(m))`; where the body can delete from the map it first checks
`v, ok := m[k]; if !ok { continue }`, which keeps Go's rule that a deleted key is not produced.

| File | Function | Change |
|---|---|---|
| `crypto.go` | package variables | `TimeNow = time.Now` and `RandReader io.Reader = rand.Reader` |
| `crypto.go` | `fileID` | `TimeNow()`; the values of the information dictionary in key order |
| `crypto.go` | `encryptAESBytes`, `calcFileEncKey`, `calcOAndUAES256`, `calcOAndUAES256Rev6` | `RandReader` for every `rand.Reader` (only the first is reached by the write fallback) |
| `info.go` | `ensureInfoDict` | `TimeNow()` (the `time` import goes) |
| `info.go` | `handleInfoDict` | key order |
| `object_frame.go` | `pushDict` | key order |
| `model/xreftable.go` | `objectDeletion.pushDict` | key order (the order of the stack, so of the objects freed) |
| `model/xreftable.go` | `BindNameTrees` | key order of `Names` |
| `types/dict.go` | `Dict.Find` | key order, for two keys that decode to the same name |
| `writeFont.go` | `prepareCoreFontObject` | key order of a font dictionary's entries |
| `writeFont.go` | `prepareCoreFontsForWriting` | object number order |
| `writeObjects.go` | `writeDirectDict`, `writeDeepDict` | key order |
| `writeObjects.go` | `writeDeepStreamDict` | key order of the stream dictionary's values |
| `optimize.go` | `optimizeFontResourcesDict`, `optimizeExtGStateResourcesDict`, `optimizeXObjectResourcesDict` | key order of the resource dictionary (which duplicate survives) |
| `optimize.go` | `optimizeXObjectForm` | the cached forms in object number order (the first equal one wins) |
| `optimize.go` | `fixCorruptFontResDicts` | key order of the dictionary, and of `Optimize.Fonts` (the first `Arial` font) |
| `optimize.go` | `CacheFormFonts` (the loop over `FormFontResDict`) | key order |
| `optimize.go` | `ensureDirectWidthForXObjs` | object number order |

The oracle sets `pdfcpu.TimeNow` to 2000-01-01T00:00:00Z (UTC) and `pdfcpu.RandReader` to a reader
that fills each request with byte `i mod 256` at position `i`, which is `WriteEnv::parity()` on the
Rust side (a reader that counted across requests would give a second IV the bytes 16.. and differ
from `parity_random`). `-gen` leaves them unset, so the encrypted inputs keep their random salts.

Map loops that were checked and left alone, because the result does not depend on the order: the
read side (`read.go`: the object table is walked in sorted order or only counted or minimised),
the validation package (loops over map literals look one key up; the others are sorted, or the
body only tests every entry), `crypto.go` `encryptDeepObject` (every entry is encrypted on its
own), `optimizeContentStreamUsage` (content stream deduplication is off), the statistics code
(`calcBinarySizes`, off) and `PropertiesAdd` (a store per key). `model/configuration.go` reads the
clock for `CreationDate`, which nothing in the output uses.

**The run-twice check.** The `go-run` stage runs the patched oracle a second time and requires the
two trees to be equal. Go seeds the iteration order of each map per process, so the second run
visits the maps in other orders; a difference is a map loop that the patch misses, and the loop
goes into the patch and this table. The check can only prove what the corpus reaches.

## Entry points of docs/scope.md section 2

Every call byblos makes is run by an operation, or is the plain access of one that is:

| Call | Run by |
|---|---|
| `ReadContext`, `ReadContextFile` (the same call on a file) | `read`, and the start of `pages`, `streams`, `budget`, `encode`, `images`, `write` |
| `Validate` | `validate` |
| `ReadAndValidate` | `rav` |
| `ReadValidateAndOptimize` | `rvo`, `propsadd`, `props`, `writeopt` |
| `Properties`, `PropertiesAdd` | `props`, `propsadd` |
| `WriteContext` | `write`, `writeopt`, `build` |
| `CreateXRefTableWithRootDict`, `CreateContext` | `build` |
| `ExtractImage`, `ColorSpaceComponents` | `images` |
| `ErrWrongPassword` | the class `wrong-password` of every operation on the `enc.*.pw` inputs |
| `EnsurePageCount`, `Pages`, `PageDict`, `InheritedPageAttrs` | `pages` (`Pages` and `Catalog` also in `build`) |
| `Dereference`, `DereferenceDict`, `DereferenceArray`, `DereferenceStreamDict` | `pages`, `rvo`, `images` (the image streams), `build` |
| `InsertObject`, `IndRefForNewObject`, `NewStreamDictForBuf` | `build`, `encode` |
| `FindTableEntry`, `FindTableEntryForIndRef`, `VersionString` | `build` (the read side uses them inside `read`) |
| `StreamDict.DecodeWithLimit`, `Encode`, `DecodeStream` | `streams`, `encode`, `budget` |
| `Rectangle`, `RectForDim` | `pages` (`rect`), `build` |
| `HexLiteral`, `StringLiteralToString`, `HexLiteralToString`, `EscapedUTF16String`, `DateTime`, `DateString` | `build` (`_build.g.txt`, section `text out`); the first three also in `rvo` and `propsadd` through the info dictionary |
| `Configuration.Limits`, `Cmd` | the configurations `d`, `g`, `s`, `b`; `Cmd` in `propsadd` (ADDPROPERTIES) and `props` (LISTPROPERTIES) |

## Known gaps

- Two runs prove the patch only where the corpus reaches. A loop on a path that no input reaches is
  not found.
- `build` and `images` have no mutated inputs of their own: `images` sees the image edits of
  `-gen` (`iw`, `ih`, `ics`, `idc`, `ism`, `ihv`, `icn`, `ica`), and `build` is fixed.
- The `write` operation runs `ReadContext` then `WriteContext` without validation, as byblos's
  fallback does; an encrypted input exercises the encrypting writer with the pinned random source.
