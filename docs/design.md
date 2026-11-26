# pdf-rs design

This is the implementation plan for pdf-rs, the Rust port of pdf-go (`github.com/dobbo-ca/pdf-go` at
`113d693`). It covers only the subset in `docs/scope.md`. It merges three area designs: A (object
model, parser, read path, XRefTable and Context, page tree, crypto), B (validate, optimize, writer,
properties) and C (filters, images, limits, threading, parity). Where they disagreed, the Go code
was re-read and the resolution is in section 1.

Paths: pdf-go paths are relative to `pdf-go/` (`pkg/...`, `internal/...`). byblos paths are relative
to `byblos/internal/pdfdoc/`. Go stdlib paths are relative to `$GOROOT/src` of Go 1.27.4. Nothing
here was compiled; every behaviour claim is read from source, and the ones that are inferences are
marked as such in section 16.

## 1. Decisions, including conflicts between the area designs

| # | Question | Decision | Evidence |
|---|---|---|---|
| 1 | What does the cumulative decode budget charge? | Go's `cap(sd.Content)`, not its length. pdf-rs models Go's slice capacity (`src/gocap.rs`). Area A said len equals cap. That holds only for object streams. | `decodeStream` charges `int64(cap(sd.Content))` (pkg/pdfcpu/model/xreftable.go:2335). `compactContent` runs only when `objStm` (:2331-2333, :2248-2252). Go tests assert the cap (model/ghsa_pr6x_vqwr_4hj2_test.go:93, :149). |
| 2 | Is Go's capacity independent of how the decoder chunks its reads? | No, not for Flate. Area C inferred that it was. Go's inflater returns the last data together with `io.EOF` or the error. `bytes.Buffer.ReadFrom` then stops without its final `grow(512)`. So the capacity depends on where Go's decompressor flushes: every 32 KiB of output, at every empty stored block, and at the end. deflate-rs has to report those flush points (prerequisite X1). | compress/flate/inflate.go:337-343 (`return n, f.err` on the last piece), :678-685 (`finishBlock` sets `toRead` and `io.EOF` together), :349-350 (a flush on error), :641-645 (the empty-stored-block flush); bytes/buffer.go:224-243. |
| 3 | Is `encoding/gob` ported? | Partly. scope.md §5 said no. The CMYK DCT path writes gob bytes into `Content` through `decodeLimitWriter`. The limit check, the cap charge and the render all depend on them. pdf-rs writes the exact gob bytes for the four `image/jpeg` result types. It renders by decoding its own gob into `image.CMYK` with gob's match-by-field-name rule. | pkg/filter/dctDecode.go:75-85, pkg/filter/filter.go:195-207, pkg/pdfcpu/writeImage.go:1119-1146. |
| 4 | Go's map order makes output and optimize choices random. How does parity work? | Rust iterates every map in sorted order. The Go oracle runs the upstream snapshot plus a small, listed determinism patch: sorted loops, a `TimeNow` hook and a `RandReader` hook. The parity job runs Go twice and fails on any difference, which proves the patch is complete. Area C's masks plus a redump are dropped, because optimize's choice of which duplicate survives cannot be masked. | Area B §3 (optimize.go:407, :626, :892); crypto.go:2358, :2371; info.go:124; crypto.go:2299. |
| 5 | Are `destination_remote.go` and `link_collect.go` reached? | Yes. All of `destination_remote.go` is reached, plus the two predicates `isEmbeddedFileSpecification` and `isURLFileSpecification`. scope.md §3.2 is corrected. | validate/action.go:52, :161, :166, :316, :373; annotation.go:868, :968; fileSpec.go:501. |
| 6 | Does `api.Validate` optimize? | Yes, when `conf.Optimize` is set, which is the default. byblos `pdfdoc.Validate` can therefore fail inside optimize. | pkg/api/validate.go:89-96. |
| 7 | PDF 2.0 write fallback | `font.EnsureCoreFontMetrics` and `internal/corefont/metrics` (1,783 lines, mostly data) are reached, so they are ported (scope §8 rule 6). | pkg/pdfcpu/writeFont.go:37, :58-99; font/fontDict.go:562. |
| 8 | TIFF output types | Both `*image.CMYK` and `*image.NRGBA` (the soft-mask path). scope.md §5 named CMYK only. | writeImage.go:358-396, :721-770 (Area C). |
| 9 | Memory ledger lifetime | One `Arc<Ledger>` per document (`Context`). Input-sized buffers are `Bytes`, an `Arc` of the bytes plus their charge. Cloning a `StreamDict` (Go's struct copy) shares them as Go's slices do, and charges nothing twice. | Go aliasing: xreftable.go:1205-1233; byblos write.go:15-21. |
| 10 | Limits field semantics | Keep Go's zero rules (0 = default, or 0 = off, per field). Fields are unsigned, so Go's "negative = unlimited" for `MaxDecodeBytes` cannot be expressed. byblos never passes a negative value (pdfdoc.go:1352). | configuration.go:554-570. |
| 11 | Randomness source | Use std only: `/dev/urandom` through `std::fs`, no `getrandom`. Only the encrypted write fallback needs random bytes. | crypto.go:2299. |
| 12 | Where the parallelism goes | Area C wanted object-stream inflate during read. Read decodes only each object stream's prolog (read.go:681-686). The full decode happens lazily, once per stream (read.go:658-671). The one parallel site is therefore a speculative prefetch of full object-stream decodes, committed in Go's order, and only if the benchmark needs it. | read.go:658-686, :3784-3817. |
| 13 | Reached but not ported, because nothing observable depends on them | Validation report, notices and logging; `writeStats.go`; optimize stats (optimize.go:1365-1590); content dedup (:46-247, off by config). | Area B §2.1, §3, §4. |

## 2. Crate layout

Package `pdf`, edition 2024, toolchain 1.99.1, with `#![forbid(unsafe_code)]`. There are no SIMD
kernels. If a benchmark later demands one, switch to the convention's `deny` and add `#[allow]` on
the kernel module only.

```
src/
  lib.rs              crate doc (Resource limits first, doc-test), re-exports
  error.rs            Error, LimitKind, Sentinel, Error::class()             errors.go, model/xreftable.go:2198-2212, filter.go:43-46
  config.rs           Config, Cmd, Limits, decode_limit, total_decode_limit  model/configuration.go:493-596, internal/streamlimit
  ledger.rs           Ledger, Held (owned), Mem, Bytes                       (tiff-rs ledger.rs, made owned)
  gocap.rs            roundupsize, ReadFrom and Write capacity simulators    bytes/buffer.go:140-269, runtime/msize.go, runtime/sizeclasses.go
  par.rs              ThreadPool, shared pool, Gauge, ordered commit         (tiff-rs par.rs without the jpeg sub-pool)
  env.rs              WriteEnv (clock, random), Go time formatting           time.Time.String, types/date.go:27
  api.rs              read, read_and_validate, validate, read_validate_and_optimize, properties, write_context
                                                                             pkg/api/api.go:81-431, validate.go, property.go
  object/
    mod.rs            Object, Ref, Rect, Name helpers                        types/types.go
    dict.rs           Dict and typed accessors                               types/dict.go
    array.rs                                                                 types/array.go
    stream.rs         StreamDict, PdfFilter, ObjectStreamDict, XRefStreamDict, LazyRef,
                      decode pipeline, encode                                types/streamdict.go
    string.rs         escape/unescape, encode_name/decode_name               types/string.go
    text.rs           UTF-16, PDFDocEncoding, EscapedUTF16String             types/utf16.go, pdfDocEncoding.go
    date.rs           DateString, DateTime(relaxed)                          types/date.go
    write.rs          pdf_string, go_string (String()), append_pdf_object    dict.go:493-539, array.go:150-197, types.go String(), writeObjects_pdf.go
  parse/
    mod.rs            lexer and object parser, xref-stream and objstm dict builders   model/parse.go
    content.rs        content-stream parser                                  model/parseContent.go
    scan.rs           Lines, LinesSingleEOL, bufio.Scanner emulation         pdfcpu/scan/scan.go, bufio/scan.go (behaviour only)
  model/
    xref.rs           XRefTable, Entry, EntryObject, find/insert/recycle, free list, charges, decode budget
                                                                             model/xreftable.go:1-1000, :2180-2345
    deref.rs          dereference family, ProcessRefCounts                   model/dereference.go
    pages.rs          PageDict walk, InheritedPageAttrs, EnsurePageCount, Catalog, Pages, PageTreeVisit
                                                                             xreftable.go:1264-1340, :2009-2754, :3002-3026; recursion.go
    resources.rs      ConsolidatePageResources, PageContent, DeleteObject/FreeObject, PageResourceNames
                                                                             xreftable.go:593-715, :2372-2470, :2858ff; model/resource.go
    names.rs          internalised name trees, BindNameTrees                 xreftable.go:1728-1800 (name-tree part)
    equal.rs          EqualObjects                                           model/equal.go
    context.rs        Context, ReadInfo, Version                             model/context.go, version.go
  read/
    mod.rs            read(), buffer()/growBufBy, object(), resolveObject, ParseObject guard   read.go:164-265, :2289-2468, :2832-2991
    xref.rs           startxref, classic xref, subsections, trailer, Prev scan  read.go:267-546, :1239-1625
    xrefstm.rs        xref streams, hybrid files                             read.go:829-1105
    repair.rs         headerVersion, bypassXrefSection, repair scanner, buildXRefTableStartingAt, readXRefTable
                                                                             read.go:1626-2290
    stream.rs         filter pipeline, stream content load, decrypt-only save   read.go:2494-2830, :3150-3551
    objstm.rs         object-stream parse, lazy entries, decode cache        read.go:560-830, :3554-3830
    deref.rs          dereference loop, hint stream, versions, encryption setup, Info materialize
                                                                             read.go:1168-1205, :3829-4515
  crypto/
    mod.rs            encrypt dict validation, password checks (empty password), key derivation,
                      decrypt and encrypt of strings and streams, fileID      crypto.go, crypto_password.go
    md5.rs  rc4.rs  aes.rs  sha2.rs  ct.rs (constant-time compare)
  filter/
    mod.rs            new_filter, copy_decoded, decode_limit_writer          filter/filter.go
    flate.rs          decode, predictors, Paeth, encode wrapper              filter/flateDecode.go, paeth.go
    lzw.rs                                                                   internal/filter/lzw/reader.go, filter/lzwDecode.go
    ascii85.rs  ascii_hex.rs  run_length.rs                                  filter/ascii85Decode.go (+ encoding/ascii85), asciiHexDecode.go, runLengthDecode.go
    ccitt.rs  ccitt_tables.rs                                                filter/ccittDecode.go, x/image/ccitt v0.47.0 (from tiff-rs)
    dct.rs                                                                   filter/dctDecode.go
    gob.rs            gob encoder for image/jpeg's 4 result types, decoder into CMYK   encoding/gob (behaviour subset)
  image/
    extract.rs        ExtractImage, ColorSpaceComponents, image entry helpers   extract.go:128-545, image.go:34, :302-361
    render.rs         renderers                                              writeImage.go:63-1186 (without WriteImage)
    png.rs            image/png encoder subset, CRC-32, bufio IDAT chunker   image/png/writer.go, bufio
    tiff.rs           uncompressed TIFF writer (CMYK, NRGBA)                 github.com/hhrutter/tiff v1.0.7 writer.go
  validate/           one file per Go file, or tiny files merged, no mode parameter
    mod.rs            xReftable.go (without links :1408-1757)
    object.rs         object.go, array_cardinality.go, graph_depth.go, tree_traversal.go
    page.rs  annotation.rs  action.rs (+form_actions)  form.rs (+form_hierarchy)  font.rs
    colorspace.rs  xobject.rs (+image_arrays)  extgstate.rs  function.rs  pattern.rs  shading.rs (+shading_arrays)
    media.rs (+mediaValidation, rendition)  name_tree.rs  number_tree.rs  outline.rs
    struct_tree.rs (+structure_hierarchy)  optional_content.rs  file_spec.rs (+2 link_collect predicates)
    destination.rs (+destination_remote)  thread.rs  three_d.rs  viewer_prefs.rs  property.rs  info.rs
    metadata.rs  active_content.rs  trapnet.rs
  annotation.rs       pdfcpu.Annotation (renderer parse used by validate)    pkg/pdfcpu/annotation.go:323ff (reached part)
  optimize/
    mod.rs            OptimizeXRefTable phases, CacheFormFonts               optimize.go (without :46-247, :1365-1590)
    frame.rs          objectStack                                            object_frame.go
    font_name.rs      font.Name, primitives.FormFontResDict                  font/fontDict.go (Name), primitives/font.go:238
  write/
    mod.rs            WriteContext, prepare, handleEncryption, xref table and xref stream   write.go
    objects.rs        object, dict, stream and object-stream writing         writeObjects.go, writeObjects_pdf.go
    pages.rs                                                                 writePages.go
    pdf20.rs          checkWrittenAnnotation, core-font preparation          writeAnnotation.go, writeFont.go
    corefont.rs       EnsureCoreFontMetrics and the metric tables (generated from Go)   font/fontDict.go:562ff, internal/corefont/metrics
  doc/
    info.rs           ensureInfoDict, handleInfoDict, writeDocumentInfoDict  info.go:57-130 and the reached helpers
    property.rs       PropertiesAdd                                          property.go:30-152
    create.rs         CreateXRefTableWithRootDict, CreateContext             createContext.go:25-99
examples/oracle.rs  examples/bench.rs
tests/parity/{go/{upstream,patches,oracle},diff.sh,bench.sh}  tests/data/  tests/fuzz/  tests/*.rs
.github/ci/Dockerfile  .github/workflows/{ci,bench,fuzz}.yml
```

Dependencies:

- `deflate` (git, rev `211232f521f4378efd74f224df2fc05213cec94a`). It only decodes; the encoder is in-crate (section 11).
- `jpeg` (git, rev `c0cf0fcc3a5c4284bf09615cea44660247f07e99`), for the decoded image types and the test encoder (section 11).
- `rayon = "=1.12.0"` for the pool, as tiff-rs uses. Justified in docs/threading.md.

There are no other dependencies.

## 3. Public API (what byblos needs)

Every entry point takes `&Limits`, directly or through `Config` or `Context`. Every long-running
call takes `cancel: &AtomicBool`.

```rust
pub struct Config { pub limits: Limits, pub cmd: Cmd }          // Default = byblos's defaults (scope §3.7, §4.1)
pub enum Cmd { Validate, AddProperties, ListProperties }         // model.CommandMode subset

// pkg/api
pub fn read(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Result<Context, Error>;                // api.go:81
pub fn read_and_validate(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Result<Context, Error>;   // api.go:93 without the file I/O; Go uses NewDefaultConfiguration, so byblos-rs passes Config::default()
pub fn validate(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Result<(), Error>;                 // validate.go:118, optimize included
pub fn read_validate_and_optimize(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Result<Context, Error>; // api.go:304, CacheFormFonts included
pub fn properties(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Result<BTreeMap<String, String>, Error>; // api/property.go:33, forces ListProperties
pub fn write_context(ctx: &mut Context, env: &WriteEnv, cancel: &AtomicBool) -> Result<Vec<u8>, Error>; // api.go:169

// pkg/pdfcpu
pub fn create_xref_table_with_root_dict(limits: &Limits) -> Result<XRefTable, Error>;    // createContext.go:25
pub fn create_context(xref: XRefTable, cfg: Config) -> Context;                          // createContext.go:87
pub fn properties_add(ctx: &mut Context, props: &BTreeMap<String, String>, env: &WriteEnv, cancel: &AtomicBool) -> Result<(), Error>; // property.go:30
pub fn extract_image(ctx: &mut Context, sd: &mut StreamDict, resource_id: &[u8], obj_nr: i64, cancel: &AtomicBool) -> Result<Option<Image>, Error>; // extract.go:511, thumb=false and stub=false fixed
pub fn color_space_components(xref: &mut XRefTable, sd: &StreamDict) -> Result<i64, Error>;  // extract.go:273
pub struct Image { pub data: Vec<u8>, pub file_type: &'static str }   // "png" | "tif" | "jpg" | "jpx" (writeImage.go:432-1179)
```

`Context` holds `pub xref: XRefTable` (Go embeds `*XRefTable`) and `pub read: Option<ReadInfo>`
(`using_object_streams`, `file_size`). The config is `ctx.xref.conf`. There is one config, so
byblos's in-place change of `MaxDecodeBytes` (pdfdoc.go:1417-1420) sees the same object Go's
`ctx.Configuration` and `xRefTable.Conf` share.

`XRefTable` has these public fields: `table: BTreeMap<i64, Entry>`, `size`, `max_obj_nr`,
`page_count`, `root`, `info`, `id`, `encrypt: Option<Ref>`, `e: Option<Enc { p, v, r, .. }>`,
`enc_key`, `header_version`, `root_version`, `conf: Config`.

Its methods follow Go's names (Area A §6):

- `find`, `find_table_entry` (the generation is ignored, xreftable.go:400-405), `find_table_entry_for_ind_ref`
- `insert_object`, `ind_ref_for_new_object`, `new_stream_dict_for_buf`
- `dereference`, `dereference_dict`, `dereference_dict_mut`, `dereference_array`
- `dereference_stream_dict`: returns an owned clone that shares the `Bytes`, and sets `entry.valid` (xreftable.go:1223-1226)
- `dereference_integer_entry`, `dereference_number`, `dereference_name_entry`
- `catalog`, `catalog_mut`, `pages`
- `page_dict(n, consolidate=false)`, `inherited_page_attrs`, `ensure_page_count`
- `version_string`, `entry_mut`
- `decode_stream`, `decode_object_stream`, `decode_image_stream`

`StreamDict` has these public fields: `dict`, `raw: Option<Bytes>`, `content: Option<Bytes>`,
`filter_pipeline`, `stream_length`, `stream_length_obj_nr`, `cs_components`. Its methods:

- `decode_with_limit(max: i64, mem: &Mem, cancel)` (streamdict.go:402)
- `decode_length_with_limit(max_len, max, mem, cancel)` (:489)
- `encode(mem, cancel)` (:289), Flate only

`Mem` is the ledger handle: `ctx.xref.mem()` for a document, or `Mem::new(&Limits)` on its own.

`Dict` mirrors Go's methods (types/dict.go):

- `find` (:95)
- `name_entry` (:194), `int_entry` (:211), `subtype`, `insert_int` (:68), `pdf_string` (:493)
- `NewDict` becomes `Dict::default()`

Text helpers: `string_literal_to_string`, `hex_literal_to_string`, `escaped_utf16_string`,
`date_time(s, relaxed)`, `date_string`.

`ErrWrongPassword`, the limit errors and `ErrDecodeLimitExceeded` are matched through `Error`
(section 5).

Mutation through aliases. Go returns the map stored in the table, so writes through it persist
(pdfdoc.go:537, buildpages.go:368, deterministic.go:149-151, properties.go:64-67,
linearize.go:230/250). Every such byblos site uses a `*_mut` accessor. `RootDict` is not cached;
it resolves through `root` on each call. The one exception is the repair case Area A §3 describes
(read.go:1759-1762), which keeps an `Option<Dict>` snapshot.

Lookups with no cancel parameter (`dereference*`) can trigger a lazy object-stream decode. For
those, an internal never-set flag is used; the decode is bounded by `max_decode_bytes` and the
ledger. Long operations call the internal `deref_c(.., cancel)` variant, which takes the caller's
flag.

## 4. Object model

Adopt Area A §2 as written:

- `Name`, `String` (the raw escaped body) and `Hex` (the digits as stored) are `Vec<u8>`.
  - The parser upper-cases hex and pads it to even length (parse.go:154-190).
  - `NewHexLiteral` writes lower case (types.go:522-524).
- `Dict` is `BTreeMap<Vec<u8>, Object>`, keyed by decoded name bytes. That is the order of Go's
  `sort.Strings` at dict.go:498-502.
- `Object` is the enum `Null | Bool | Integer(i64) | Float(f64) | Name | String | Hex | Ref(Ref) | Array | Dict | Stream(Box<StreamDict>)`.
- Table entries hold `EntryObject = Obj | ObjStm | XRefStm | Lazy { stm, start, end }`.

Serialization:

- `pdf_string` follows Area A's rules exactly:
  - spacing per type (dict.go:493-539, array.go:150-197);
  - `FormatFloat('f', 12)`;
  - a `Stream` nested in a dict or array is dropped silently, because Go's `default:` drops it.
- `go_string` ports each type's `String()` method. `fileID` hashes it (crypto.go:2371-2376).

`Bytes` (ledger.rs):

- It is `Arc<Charged { v: Vec<u8>, go_cap: usize, _held: Option<Held> }>`.
- Clones share, as Go's slices do.
- `Content` that aliases `Raw` (`preserveRawContent`, streamdict.go:500-503) is the same `Arc`.
  `decodedContentAliasesRaw` (xreftable.go:2241-2243) is then `Arc::ptr_eq`, plus both lengths
  greater than 0.

## 5. Errors

There is one `Error` enum. Go errors that satisfy two sentinels have predicates that mirror
`errors.Is`.

| Rust | Go | `is_limit_exceeded` | `is_decode_limit_exceeded` | class token |
|---|---|---|---|---|
| `WrongPassword` | `pdfcpu.ErrWrongPassword` (read.go:81) | | | `wrong-password` |
| `Limit(ObjectStreamEntries)` | `ErrLimitExceeded` from xreftable.go:2188 | yes | | `limit/objstm-entries` |
| `Limit(Parsed)` | `ErrParsedLimitExceeded` (:2205) | yes | | `limit/parsed` |
| `Limit(ObjectBytes)` | `ErrObjectBytesExceeded` (:2209); also `is(ObjectBufferLimit)` (read.go:2383) | yes | | `limit/object-bytes` |
| `Limit(StreamTotal)` | `ErrStreamTotalLimitExceeded` (:2212) | yes | | `limit/stream-total` |
| `Limit(DecodeBudget { objstm })` | `decodeBufferBudgetError` (:2257-2264) | yes | yes | `limit/total-decode` |
| `Limit(ObjectCount \| XRefEntries \| ...)` | reserved, not returned today: the read path gives an untyped `Corrupt` for these, as Go does (src/error.rs) | yes | | `limit/objects` |
| `DecodeLimit` | `filter.ErrDecodeLimitExceeded` (filter.go:46), one stream | | yes | `limit/decode` |
| `XRefObjectRange` | "xref subsection object range exceeds limit" (read.go:466, :480), which wraps `errCorruptXRefSubsection` | no | | `limit/xref-range` |
| `ImageLimit { pixels: bool }` | untyped (writeImage.go:132, :145) | no | | `limit/image-pixels`, `limit/image-bytes` |
| `NoContent` | `model.ErrNoContent` (xreftable.go:46) | | | `no-content` |
| `UnsupportedFilter(name)` | `filter.ErrUnsupportedFilter` (filter.go:43) | | | `unsupported-filter` |
| `InvalidFilter(name)` | "invalid filter" from NewFilter | | | `invalid-filter` |
| `UnsupportedResource` | `pdfcpu.ErrUnsupportedResource` (errors.go:79) | | | `unsupported-resource` |
| `UnsupportedEncryption(..)` / `MalformedEncryption(Box<Error>)` | crypto.go validation; `errors.Join` (crypto.go:151) | | | `encryption/unsupported`, `encryption/malformed` |
| `Flate(deflate::Error)` | `flate.CorruptInputError`, `zlib.ErrHeader`, `zlib.ErrChecksum` | | | `flate/corrupt`, `zlib/header`, `flate/checksum` |
| `UnexpectedEof` | `io.ErrUnexpectedEOF` that is not swallowed | | | `unexpected-eof` |
| `Lzw`, `Ascii85(off)`, `Hex(byte)`, `Ccitt(..)`, `Predictor(..)`, `Jpeg(jpeg::Error)`, `Gob(..)` | the filter errors | | | `lzw/invalid-code`, `ascii85/corrupt`, `ascii85/eod`, `hex/invalid-byte`, `ccitt`, `predictor/params`, `predictor/rows`, `jpeg/format`, `jpeg/unsupported`, `gob` |
| `Sentinel(Sentinel)` | internal sentinels that are tested with `errors.Is` (see below) | | | `syntax` |
| `Validation(String)` | errors from validate | | | `validate` |
| `Corrupt(String)` | untyped `fmt.Errorf` in read and model | | | `syntax` |
| `Panic(&'static str)` | a site where Go panics (index out of range, nil dereference) | | | `panic` |
| `Context(Box<str>, Box<Error>)` | `%w` wrapping; transparent to every predicate | | | the inner error's |
| `Alloc { got, max }`, `OutOfMemory`, `Cancelled` | Rust only, and `c.Err()` | | | `limit/alloc`, `oom`, `cancelled` |

The `Sentinel` set comes from Area A §10:

- `MissingTrailerDict`, `UnregisteredObject`, `CorruptObjectOffset`, `ObjectReadBudget`
- `CircularRef`, `MaxRecursionDepth`, `TruncatedStreamMarker`, `XRefStreamIndexSizeMismatch`
- `NilDereferenced`, `ReferenceDoesNotExist`, `AesTooShort`, `AesUnaligned`
- `Eof`, `MissingXRefSection`, `MissingXRefEof`, `ObjectBufferLimit`
- `CorruptXRefSubsection` (also true for `XRefObjectRange`), `NameObjectCorrupt`, `CorruptFontDict`

The read path branches on these exactly as Go does: read.go:2155-2160, :3922-3928, :4220-4232,
:1971.

Classes are assigned from the most specific case down, in the table's order (the
`DecodeBudget` row comes before `DecodeLimit`). An unmapped Go error becomes `other`, and CI fails
on `other`. The mapping goes in `docs/parity.md`. byblos's check order (pdfdoc.go:100-111) maps
one-to-one onto these predicates. `XRefObjectRange` replaces byblos's string match at
pdfdoc.go:463.

## 6. Limits (`src/config.rs`)

These mirror `model.ResourceLimits` field for field (configuration.go:493-550, defaults :573-591),
plus the two fields every port shares. All byte fields are `u64` bytes, count fields are `u64`,
and `max_threads` is `usize`.

| Field | Go field | Default | Unit and meaning (Go's rule kept) |
|---|---|---|---|
| `max_input_bytes` | MaxInputBytes | 0 | bytes; 0 = no cap |
| `max_object_bytes` | MaxObjectBytes | 64 MiB | bytes; 0 = default. Only a value different from the default gives the typed `Limit(ObjectBytes)`. At the default the error is the untyped `ObjectBufferLimit`, and relaxed read skips the object (read.go:2378-2391) |
| `max_stream_bytes` | MaxStreamBytes | 512 MiB | bytes for one stream read without a length |
| `max_decode_bytes` | MaxDecodeBytes | 512 MiB | decoded bytes per stream; 0 = 512 MiB (:554-559) |
| `max_total_decode_bytes` | MaxTotalDecodeBytes | 0 | cumulative Go capacity (section 8); 0 = `DecodeAllowance(decode)`: 1 GiB if decode ≤ 512 MiB, else 2× (saturating) (:565-570) |
| `max_image_pixels` | MaxImagePixels | 100,000,000 | pixels |
| `max_image_bytes` | MaxImageBytes | 512 MiB | bytes, raw and 4 bytes per pixel |
| `max_object_count` | MaxObjectCount | 10,000,000 | objects |
| `max_object_stream_count` | MaxObjectStreamCount | 1,000,000 | `/N` of one stream |
| `max_object_stream_total_count` | MaxObjectStreamTotalCount | 0 | entries over all object streams; 0 = off |
| `max_parsed_bytes` | MaxParsedBytes | 0 | estimated bytes (constants 64/96/256, xreftable.go:2198-2202); 0 = off |
| `max_stream_total_bytes` | MaxStreamTotalBytes | 0 | encoded stream bytes read; 0 = off |
| `max_object_stream_first` | MaxObjectStreamFirst | 16 MiB | bytes |
| `max_xref_entries` | MaxXRefEntries | 10,000,000 | entries |
| `max_recursion_depth` | MaxRecursionDepth | 100 | levels; 0 = 100 (recursion.go:108-123) |
| `max_alloc` | (Rust only) | 4 GiB | the peak bytes held by one document plus the call in progress (section 7) |
| `max_threads` | (Rust only) | `available_parallelism()` | threads per call, the caller included; 1 = no spawn, no pool (every call is serial today) |

Two budgets are internal and cannot be configured:

- the object read budget, `MaxObjectBytes + 8 × file size` (read.go:2801-2830);
- the separate object-stream decode allowance, used when `max_total_decode_bytes == 0` (xreftable.go:2310-2313).

The fixed configuration is constants, not fields: relaxed validation, `ValidateLinks=false`,
`WriteObjectStream=true`, `WriteXRefStream=true`, `Optimize=true`, `OptimizeResourceDicts=true`,
`OptimizeDuplicateContentStreams=false`, `DecodeAllStreams=false`, and empty passwords. The values
are from configuration.go:882-896.

## 7. Ledger and memory

**The ledger.**

- `Ledger` is tiff-rs's ledger.rs with one change: `Held` owns an `Arc<Ledger>` instead of
  borrowing it, so a charge can live in the `Context` between byblos calls.
- `read` creates the ledger from `limits.max_alloc` and stores it in the `XRefTable`.
- `Mem` is the cloneable handle to it.

**What is charged, and when it is released.**

| Memory | Charged | Released |
|---|---|---|
| `Raw` and `Content` buffers, and object-stream decode results | when the `Bytes` is made | when the last clone drops |
| Parsed objects | Go's `chargeParsed` estimate, always, even when `max_parsed_bytes == 0`, to `XRefTable::held` (`ParsedCounter` also counts it against `max_parsed_bytes` when that is on) | when the `XRefTable` drops |
| Cross-reference entries, xref-stream object lists, optimizer page sets | a fixed estimate per new entry (`ENTRY_BYTES`), 8 bytes per object number, 64 bytes per page-set access, to `XRefTable::held` | when the `XRefTable` drops |
| Copies of an encrypted string | 7 x its length + 192 | when the string is done |
| Object buffers in `buffer()`, filter intermediates, rasters, PNG/TIFF encoder state | per call | when the call ends |
| Returned PNG/TIFF bytes | | when they are handed back |

**`max_alloc` and parity.** `max_alloc` is a backstop. Its 4 GiB default sits above the peak that
Go's own limits allow at Go's defaults: 512 MiB of stream, 1 GiB of decode allowance, the input,
and the render at 4 bytes per pixel times 100 MP. The parity configurations set it above the Go
oracle's `GOMEMLIMIT`, so it never trips in parity. Rust-only tests run with small `max_alloc`
values and check the ledger's peak.

**Nested crates.** Before a nested crate runs, pdf-rs charges its working memory and passes the
charge as the crate's `max_alloc`. deflate-rs: `min(left, 256 KiB)` (the reader's window), passed as its `max_alloc`; the output is
charged as it grows and a 32 KiB buffer is charged separately. deflate-rs is the only nested crate
and has no `max_threads`. `gojpeg` (in-crate) charges its planes, progressive coefficients and the
RGBA/CMYK output on the document ledger before it allocates them; jpeg-rs is no longer called to decode.

**Rule.** Nothing is allocated from an input-controlled size before that size passes the limits.
Allocation uses `try_reserve_exact`.

## 8. Go capacity model (`src/gocap.rs`)

`cap(sd.Content)` feeds the cumulative budget (xreftable.go:2335). Every `Bytes` carries `go_cap`.
The model below is derived from bytes/buffer.go (Go 1.27.4) and runtime/msize.go plus
sizeclasses.go (BSD; listed in NOTICE).

- **Write path** (`bytes.Buffer.Write` and `WriteByte`): used for predictor rows (flateDecode.go:525-538), RunLength, gob messages and the PNG/TIFF output.
  - A nil buffer with `n <= 64` gets capacity 64 (buffer.go:154).
  - Otherwise, when the buffer is full, the capacity becomes `roundupsize(max(len+n, 2*cap))` (`growSlice`).
- **ReadFrom path** (`io.Copy` into a `bytes.Buffer`, filter.go:164-193, streamdict.go:379-391): `simulate_read_from(segments, end_carried)`.
  - Before each read, call `grow(512)`. The first allocation is 512. Later ones are `roundupsize(max(len+512, 2*cap))`.
  - Each read takes `min(cap - len, rest of the current Go segment)`.
  - The read that brings the end or error with data stops without another grow.
  - Segments are the decoder's Go `Read` boundaries.
- **Exact sizes:** `make([]byte, n)` (ASCIIHex, asciiHexDecode.go:97) gives capacity exactly `n`. Compacted object-stream content has capacity equal to its length (xreftable.go:2248-2252).

**Segments per decoder.** A decoder that ends with a separate `(0, EOF)` read has a closed form:
the smallest `512·2^k` that is at least `len + 512`. Each filter port documents its segmentation,
and the oracle's `.cap` output checks it.

- **Flate passthrough** (zlib, no predictor): boundaries at every 32,768 output bytes, at each empty stored block (inflate.go:641-645), and the final segment, which carries EOF or the swallowed `UnexpectedEOF` (inflate.go:337-350, :678-685). This needs flush marks from deflate-rs (X1).
- **LZW** (internal/filter/lzw/reader.go): ends with a separate error read, per its `Read` loop. To verify.
- **ASCII85** (encoding/ascii85 Decoder): to verify.
- **CCITT** (x/image/ccitt reader): to verify.
- **RC4 decrypt of `Raw`** (`io.Copy` from a `cipher.StreamReader` over a `bytes.Reader`, crypto.go:2261-2277): closed form.

## 9. Threading

**Verified:** no in-scope Go code starts a goroutine (Area C grep), and Go's budgets are
order-dependent. So:

- **Phase 1: everything serial.** `max_threads` is accepted and documented. Output is trivially
  identical for every value.
- **Phase 2, only if the benchmark is above 1.10× Go on the objstm or validate classes:** a
  speculative full-decode prefetch of object streams (WP 29).
  - Workers decode with the per-stream limit, keeping partial output on error.
  - The calling thread commits each result at the point where Go first touches that stream (read.go:658-671). There it applies `ChargeObjectStreamEntries`, the cap budget and `MaxParsedBytes`.
  - When Go's effective limit `L'` is below the prefetch limit, the commit rule gives the same answer as a serial decode at `L'`: if the partial output is longer than `L'`, the result is Go's limit error; otherwise it is the prefetched result. This works because Go stops reading at `L' + 1` (filter.go:185-191).
  - A worker that cannot charge the ledger drops its prefetch, and that stream decodes serially. Speculation never creates an error.
- **Pool:** tiff-rs par.rs without the jpeg sub-pool. Unused today: no entry point takes a pool. `max_threads = 1` must never touch the pool.
- **Serial on purpose:** validation, optimize, the writer, `extract_image` (it changes the shared budget) and PNG deflate (bit-exact).

## 10. Determinism

- **Rust:** every map is a `BTreeMap`, or is visited in sorted order. Dict keys are ordered by
  decoded name bytes, object numbers ascend, and struct keys go by `(num, gen)`. Time and
  randomness come only from `WriteEnv { now: GoTime { unix, utc_offset_min }, random: fn(&mut [u8]) -> Result<(), Error> }`.
  - `Default` uses `SystemTime` in UTC and reads `/dev/urandom`. go-differences.md records UTC
    instead of local time, and no Windows support.
  - `WriteEnv::parity()` is pinned to 2000-01-01T00:00:00Z, with random bytes `i mod 256` counted
    per operation.
  - Rust reproduces `time.Time.String()` for a time with no monotonic reading:
    `2000-01-01 00:00:00 +0000 UTC`.
- **Go oracle patch** (`tests/parity/go/patches/determinism.patch`; every hunk listed in
  docs/parity.md):
  - **Sorted loops:**
    - writeObjects.go:641, :735, :769
    - writeFont.go:60, :82
    - crypto.go:2371
    - optimize.go:407, :626, :811, :892, :1294, :1296, :1711
    - object_frame.go:57
    - model/xreftable.go:715, :1734
    - types/dict.go:100
    - info.go:60
    - every further site that the run-twice check finds, including any in read, model or types (Area A found none besides dict.go:100 and crypto.go:2371).
  - A sorted loop over a map that the body mutates re-checks `if _, ok := m[k]; !ok { continue }`. That keeps Go's rule that a deleted key is not produced. A key inserted during the loop is not visited, which Go allows.
  - **Hooks:** `var TimeNow = time.Now`, used at info.go:124 and crypto.go:2358; `var RandReader io.Reader = rand.Reader`, used at crypto.go:2299.
  - The oracle sets both hooks to the same fixed values as `WriteEnv::parity()`.
- **Why this is legitimate:** Go leaves map iteration order unspecified, so sorted order is one
  allowed execution of the same program.
- **Completeness check:** the parity job runs the patched Go oracle twice. The two runs use
  different map seeds, which Go randomises per run. Any difference fails the job.

## 11. Cross-repository prerequisites

Owner rule: another -rs repo gets at most a small PR, and only when pdf-rs needs it. So:

- **X1 (encoder) moves into pdf-rs.** `src/filter/goflate/` ports Go 1.27 `compress/flate`
  (deflate.go default-level path, level6.go, huffman_bit_writer.go, huffman_code.go, token.go)
  and `compress/zlib` writer. It emits Go's exact `Write` calls to a sink
  `FnMut(&[u8]) -> Result<(), Error>`, because PNG's IDAT chunks follow them
  (image/png/writer.go:547-556; `bufferFlushSize`, huffman_bit_writer.go:35). NOTICE carries the
  Go BSD licence for these files. Its own unit tests use Go-generated golden vectors written by
  the oracle's `-gen` (pooled-and-Reset writers against fresh ones, flateDecode.go:110-125, and
  chunked `Write` calls against one call).
- **X1' (decode flush marks) is a small deflate-rs PR**, only if deflate-rs cannot report them
  already: `zlib::decode_limit_into_marks(.., marks: &mut Vec<usize>)`, which records the output
  offsets of Go's `toRead` flushes for section 8. WP 4 first checks whether `deflate::Reader`
  already returns Go's chunk boundaries; if it does, no PR.
- **X2 moves into pdf-rs.** `src/filter/gojpeg.rs` ports Go's `image/jpeg` decoder whole:
  reader.go (byte buffer, unstuffing and unread, marker loop, SOF with flex sampling, DQT, DRI,
  APP0/APP14, colour output), huffman.go, scan.go (baseline and progressive scans, restarts,
  reconstruction) and dct.go (`idct`), with Go's `FormatError` and `UnsupportedError` messages.
  An entropy decoder with libjpeg semantics (jpeg-rs's `decode_coefficients`) differs from Go on
  corrupt data (a run past `zigEnd`, baseline EOB runs), so it is not used. jpeg-rs still
  supplies the result image types and the test encoder. Go BSD licence in NOTICE.

byblos-rs can later use `pdf::filter::goflate` for its own `compress/zlib` call
(deterministic.go:349). For its `zlib.BestSpeed` and `png.Encoder` at `BestCompression` calls,
`ZlibWriter::with_level` adds level 1 (level1.go, `src/filter/goflate/level1.rs`) and level 9
(deflate.go `deflateLazy`, `fillDeflate`, `findMatch`, `src/filter/goflate/lazy.rs`); other
levels are refused. Parity: the `zlevel` op (docs/parity.md).

## 12. Parity oracle

The layout copies tiff-rs:

- `tests/parity/go/upstream`: the pdf-go non-test sources at 113d693, plus go.mod, LICENSE and NOTICE.
- `tests/parity/go/patches/determinism.patch`: applied in the `go-oracle` Docker stage.
- `tests/parity/go/oracle`: a Go `main` with its own go.mod that `replace`s the upstream.

pdf-go has no private imports (go.mod), so no `deps/` directory is needed. `GOMEMLIMIT` bounds the
Go oracle.

**Limit configurations (both sides):**

- `d`: byblos's defaults (pdfdoc.go:81-93), copied as constants with that reference.
- `g`: `DefaultResourceLimits()`.
- `s`: tight limits:
  - decode 64 KiB, total 256 KiB
  - parsed 1 MiB, objstm entries 1,000, stream total 1 MiB
  - image pixels 1 M, object bytes 1 MiB (the typed path)
- `b`: boundary values from the `<name>.limits` sidecar that `-gen` writes. Examples: `max_decode_bytes` at len−1, len and len+1; `max_total_decode_bytes` at Σcap−1 and Σcap.

**Operations and files**, per input `<n>` and configuration `<c>`. An error writes `.err` with the
class token.

| Op | Go calls | Files |
|---|---|---|
| `read` | `api.ReadContext` | `<n>.<c>.read.dump`: canonical dump. Per object number ascending: gen, free, and `PDFString`, or `stream raw <len>:<sha256> content <len>:<sha256>\|nil`. Also Root, Info, ID, Encrypt, P/V/R, header and root version, PageCount, Size, UsingObjectStreams |
| `pages` | `EnsurePageCount`, `PageDict(n,false)`, `InheritedPageAttrs` for up to 256 pages | `<n>.<c>.pages.out` (one line per page, or a class) |
| `streams` | `DecodeWithLimit` on a fresh copy, for up to 512 streams | `<n>.<c>.s<obj>.out`, `.s<obj>.cap` |
| `budget` | `XRefTable.DecodeStream` over all streams in object order | `<n>.<c>.budget.out` (`obj ok cap` or `obj class`) |
| `encode` | `NewStreamDictForBuf` + `Encode` on each decoded content and on `pkg/filter/testdata/*.txt`, in one process | `<n>.<c>.e<obj>.out` |
| `images` | `ColorSpaceComponents`, `ExtractImage`, and `cap(sd.Content)` after it, for up to 64 images | `<n>.<c>.images.out`, `<n>.<c>.i<obj>.<type>.out` |
| `validate` | `api.Validate` | `<n>.<c>.validate.ok` or `.err` |
| `rvo` | `ReadValidateAndOptimize` (Cmd=VALIDATE) | `<n>.<c>.rvo.dump` (the read dump plus the `Names` keys and whether `Form` is set) |
| `propsadd` | `ReadValidateAndOptimize` (ADDPROPERTIES) + `PropertiesAdd({"Title":"parity","Key":"Wert ü"})` | `<n>.<c>.propsadd.dump` |
| `props` | `api.Properties` | `<n>.<c>.props.out` (sorted `k=v`) |
| `write` | `ReadContext` → `WriteContext` | `<n>.<c>.write.out` (exact bytes), `<n>.<c>.write.redump` (read of the output) |
| `writeopt` | `rvo` → `WriteContext` | `<n>.<c>.writeopt.out` |

**Every Go operation runs under `recover()`.** A panic is written as `panic`, and Rust returns
`Error::Panic` at the same site. byblos recovers these panics itself (pdfdoc.go:151, :1341).

**`examples/oracle.rs`** mirrors this: `oracle [-threads N] <inputs> <outputs>`, the same
configurations, operations, file names and dump format. It runs with `-threads 1` and the default,
and the two outputs must be identical. `tests/parity/diff.sh` runs `diff -r`.

**`-gen` mutations** (Go, seeded by file name). These follow Area C, with the sidecar `.limits`
files for `b`.

1. **Generic:** 8 truncation points (inside the last xref, inside the first stream), 16 bit flips, and deletions next to `stream` and `endobj`.
2. **Structural:**
   - `startxref` ±1, 0 and huge; xref entry offsets; `/Size` −1 and huge.
   - `/Length` ±1, 0, huge, and indirect to a free object; bit flips inside a stream body only.
   - `/Filter` swaps: unknown, JBIG2 in mid-pipeline, Crypt first.
   - A DecodeParms grid:
     - Predictor {1, 2, 10-15, 16, 99}
     - Colors {0, −1, 2^20}, BPC {3, 16, 32}, Columns {0, −1, 2^30}
     - EarlyChange 0, K {1, −1}, BlackIs1, EncodedByteAlign, Rows 0
   - ObjStm `/N` and `/First`; XRef `/W`.
   - Root removed; nesting to depth 101.
   - Image edits: Width, Height, BPC, ColorSpace, Decode, SMask pointing to a non-stream, Indexed hival {0, 255, 256} with a short lookup, ICCBased `/N` {2, 5}.
3. **Synthesized PDFs:**
   - A filter × parameters × {valid, truncated, corrupt} matrix, encoded with pdf-go's own encoders, including predictor encode through `encodePreProcess`.
   - Flate streams with sync-flush markers (empty stored blocks) for the cap model.
   - An image matrix that reaches every `render*`.
   - A DCT matrix: CMYK, Adobe YCCK, and YCbCr, gray and RGB labelled DeviceCMYK; progressive; truncated. The CMYK JPEGs come from jpeg-go testdata.
   - PDF 2.0 with core fonts and annotations, for the `write` path.
4. **Encrypted variants** made with the snapshot's `api.Encrypt` (fine for the generator):
   - RC4-40, RC4-128, AES-128 and AES-256 with an empty user password;
   - a non-empty user password (expects `wrong-password`);
   - byte edits to `/O`, `/U`, `/P`, `/OE`, `/UE` and `/Perms`.

**Corpus:**

- `pkg/testdata`: all 56 top-level files plus `pdf20/` and `image-fixtures/test.pdf`, copied to `tests/data`.
- A subset of about 60 files from `pkg/samples`.
- The Go tests listed in Area C, ported.

## 13. Fuzz targets (`tests/fuzz`)

pdf-go has no `Fuzz*` functions and no `testdata/fuzz`. Seeds are the small testdata PDFs (under
300 KB), the `-gen` outputs and `filter/testdata/*.txt`.

| Group | Targets |
|---|---|
| Document | `read`, `read_validate`, `rvo`, `write_fallback` |
| Images | `extract_image` (the image dict comes from header bytes), `filter_dct_cmyk` |
| Filters | `filter_flate`, `filter_lzw`, `filter_ascii85`, `filter_asciihex`, `filter_runlength`, `filter_ccitt` |
| Round trips | `png_encode` (round trip through deflate-rs inflate and unfilter), `gob` (encode, then decode-into-CMYK) |
| Model | `gocap` (the closed form against a step-by-step `bytes.Buffer` simulation), `parse_object`, `parse_content` |

Each target runs with `-rss_limit_mb=2048 -malloc_limit_mb=512`.

## 14. Benchmark classes (Go vs Rust, default threads and 1 thread)

| Class | Inputs |
|---|---|
| `read/small` | test.pdf, empty.pdf, testRot.pdf |
| `read/objstm` | CenterOfWhy.pdf, adobe_supplement… |
| `read/large` | WaldenFull.pdf, FOSDEM14….pdf, gobook.0.pdf |
| `validate`, `rvo` | the same files |
| `write/fallback` | a generated AES-256 file, a generated RC4 file |
| `decode/flate+predictor` | all streams of a large file |
| `encode/flate` | Mark.Twain.txt, pi.txt, e.txt |
| `image/png`, `image/tiff`, `image/dct-cmyk`, `image/ccitt` | testImage.pdf, a CMYK image, a CMYK DCT image, T6.pdf / HL1396.pdf |

The target is at most 1.10× Go on every class.

## 15. Work packages, in order

Sizes are Rust lines including unit tests. "Dep" lists the packages that must be merged first.
Each package ports its Go tests and ends with green `test` CI. From WP 10 on, each also passes the
parity operations it enables.

| WP | Content | Rust files | Go ported | Size | Dep |
|---|---|---|---|---|---|
| X1 | pdf-rs: Go zlib encoder at the default level with a write sink (in-crate, see section 11); flush marks via deflate-rs only if needed | `src/filter/goflate/*` | compress/flate deflate.go (fast path), level6.go, huffman_bit_writer.go, huffman_code.go, token.go, zlib/writer.go | 2,600 | none |
| X2 | pdf-rs: Go image/jpeg decoder, whole (in-crate; jpeg-rs supplies only the image types and test encoder) | `src/filter/gojpeg.rs` | image/jpeg idct.go and reader.go DecodeConfig | 600 | none |
| 1 | Skeleton: Cargo, lib docs, error, config, ledger, `Bytes`, gocap, par (serial), env, NOTICE/LICENSE, CI Dockerfile and workflows | lib.rs, error.rs, config.rs, ledger.rs, gocap.rs, par.rs, env.rs | configuration.go:493-596, streamlimit, errors.go, bytes/buffer.go growth, msize.go | 1,600 | none |
| 2 | Object model and serializers | object/* | types/types.go, dict.go, array.go, string.go, utf16.go, pdfDocEncoding.go, date.go, streamdict.go (structs, lazy entry, ObjectStreamDict and XRefStreamDict without decode), writeObjects_pdf.go | 2,600 | 1 |
| 3 | Parser and scanners | parse/mod.rs, parse/scan.rs | model/parse.go, pdfcpu/scan/scan.go | 2,200 | 2 |
| 4 | Filters I and the stream decode pipeline | filter/{mod,flate,lzw,ascii85,ascii_hex,run_length}.rs, object/stream.rs (decode, encode) | filter.go, flateDecode.go, paeth.go, lzwDecode.go, internal/filter/lzw/reader.go, ascii85Decode.go, asciiHexDecode.go, runLengthDecode.go, streamdict.go:249-576 | 2,600 | 1, 2, X1 (encode and marks; decode can land first with encode stubbed) |
| 5 | XRefTable core, dereference, Context | model/{xref,deref,context}.rs | xreftable.go:1-1000, :1205-1340, :2180-2345; dereference.go; recursion.go:100-130; context.go; version.go | 2,500 | 3, 4 |
| 6 | Read I: header, xref, trailer, repair, buffer, object | read/{mod,xref,xrefstm,repair}.rs | read.go:1-546, :829-2470, :2832-2991 | 2,900 | 5 |
| 7 | Crypto: primitives, validation, keys, decrypt and encrypt | crypto/* | crypto.go (decrypt, plus encryptDeepObject, encryptStream, encryptAESBytes and fileID), crypto_password.go (empty path) | 2,600 | 2, 5 |
| 8 | Read II: streams, object streams, lazy objects, dereference loop, versions, encryption setup | read/{stream,objstm,deref}.rs | read.go:546-830, :2494-2830, :3150-4515 | 2,800 | 6, 7 |
| 9 | Page tree and the `read` API | model/pages.rs, api.rs (read) | xreftable.go:1264-1340, :2009-2754, :3002-3026; recursion.go:137-170; api.go:81-115 | 1,700 | 8 |
| 10 | Parity harness I: Go oracle, `-gen`, Rust mirror for read, pages, streams, budget, encode; diff and Docker stages; docs/parity.md | tests/parity/**, examples/oracle.rs | (Go) | 1,800 Rust + Go | 9 |
| 11 | Filters II: CCITT, CMYK DCT and gob | filter/{ccitt,ccitt_tables,dct,gob}.rs | ccittDecode.go, x/image/ccitt (from tiff-rs, plus Invert, Align and auto height), dctDecode.go, gob subset | 1,700 | 4, X2 |
| 12 | Image extraction, renderers, PNG and TIFF writers; `images` op | image/*, api (extract_image, color_space_components) | extract.go:128-545, image.go, writeImage.go:63-1186, image/png writer subset, hhrutter/tiff Encode subset | 2,900 | 9, 11, X1 |
| 13 | Validate V1: driver, root table, info, XMP, properties, viewer prefs | validate/{mod,info,metadata,property,viewer_prefs}.rs | xReftable.go (without :1408-1757), info.go, metaData.go, property.go, viewerPreferences.go | 2,500 | 9 |
| 14 | Validate V2: generic validators | validate/{object,active_content,trapnet}.rs | object.go, array_cardinality.go, graph_depth.go, tree_traversal.go, active_content.go, trapnet_states.go | 2,100 | 13 |
| 15 | Validate V3: pages, optional content | validate/{page,optional_content}.rs | page.go, optionalContent.go | 2,300 | 14 |
| 16 | Validate V4: annotations | validate/annotation.rs, annotation.rs | validate/annotation.go, pdfcpu/annotation.go:323ff (reached part) | 2,700 | 14 |
| 17 | Validate V5: actions, destinations, file specs | validate/{action,destination,file_spec}.rs | action.go, form_actions.go, destination.go, destination_remote.go, fileSpec.go, link_collect.go (2 predicates) | 2,700 | 14 |
| 18 | Validate V6: forms, fonts | validate/{form,font}.rs, optimize/font_name.rs | form.go, form_hierarchy.go, font.go, font/fontDict.go `Name`, primitives/font.go:238 | 2,700 | 14 |
| 19 | Validate V7: XObjects, colour spaces | validate/{xobject,colorspace}.rs | xObject.go, image_arrays.go, colorspace.go | 2,400 | 14 |
| 20 | Validate V8: ExtGState, functions, patterns, shadings | validate/{extgstate,function,pattern,shading}.rs | extGState.go, function.go, pattern.go, shading.go, shading_arrays.go | 2,600 | 14 |
| 21 | Validate V9: media, 3D, threads, outlines | validate/{media,three_d,thread,outline}.rs | media.go, mediaValidation.go, rendition.go, threeD.go, thread.go, outlineTree.go | 2,700 | 14 |
| 22 | Validate V10: name and number trees, structure tree; `validate` API | validate/{name_tree,number_tree,struct_tree}.rs, api.rs (validate, read_and_validate) | nameTree.go, numberTree.go, structTree.go, structure_hierarchy.go, api.go:117-145, :236-300 | 2,600 | 14 (the API part after 13-21) |
| 23 | Optimize I: content parser, equality, resources, object deletion | parse/content.rs, model/{equal,resources}.rs | parseContent.go, equal.go, resource.go, xreftable.go:593-715, :2372-2470, :2858ff | 2,200 | 9 |
| 24 | Optimize II: OptimizeXRefTable, objectStack, CacheFormFonts; `read_validate_and_optimize` | optimize/{mod,frame}.rs, api.rs | optimize.go (without :46-247 and :1365-1590), object_frame.go, api.go:145-170, :290-340 | 2,000 | 18, 22, 23 |
| 25 | Writer I: WriteContext, objects, object streams, xref, pages, name trees | write/{mod,objects,pages}.rs, model/names.rs | write.go, writeObjects.go, writePages.go, xreftable.go:1728-1800 | 2,900 | 7, 9, X1 |
| 26 | Writer II and document: info, properties, create, PDF 2.0, core fonts; `properties` and `write_context` API | doc/*, write/{pdf20,corefont}.rs, api.rs | info.go (reached part), property.go, api/property.go, createContext.go:25-99, writeAnnotation.go, writeFont.go, fontDict.go:562ff, internal/corefont/metrics (generated data) | 2,800 (about 1,200 of it data) | 24, 25 |
| 27 | Parity harness II: determinism patch, the validate, rvo, propsadd, props, write and writeopt ops, run-twice check | tests/parity/go/patches, oracle ops, examples/oracle.rs | (Go) | 1,200 | 10, 26 |
| 28 | Fuzz targets, bench, docs (limits, threading, parity, go-differences, safety), README | tests/fuzz/*, examples/bench.rs, docs/*, README.md | (none) | 1,500 | 27 |
| 29 | Threading phase 2, only if WP 28's benchmark needs it: object-stream prefetch with ordered commit | par.rs, read/objstm.rs | (none) | 500 | 28 |

**Parallelism between packages.**

- X1 and X2 run alongside WP 1-3.
- WP 11 can start after WP 4.
- WP 15-21 can run in parallel once WP 14 is merged.
- WP 23 can run alongside the validate packages.
- WP 25 can start after WP 9.

**Total:** about 66,000 Rust lines in pdf-rs plus 3,200 in the two prerequisite repos. Validate is
about 25,000 of those.

## 16. Inferences and gaps (to be settled by the named test)

**Not verified; the oracle `.cap` output settles each:**

- the per-filter read segmentation for LZW, ASCII85 and CCITT;
- that Go's flate decompressor has no other flush points than the three in section 8;
- the gob message split (one `Write` per message, encoding/gob/encoder.go:64-89, read; the split of type versus value messages is not read).

**Not checked; Go-generated golden tests settle each:**

- the exact gob type-descriptor bytes;
- whether gob omits zero `Rect` and `Point` fields;
- whether gob's YCbCr-into-CMYK decode matches only `Rect` (expected: yes, then `At` panics).

**Compression and float formatting:**

- Go's flate output may depend on how input is chunked into `Write` calls, and a pooled `Reset` writer may differ from a fresh one (flateDecode.go:110-125). X1's parity job settles both.
- The arch-specific `reg8SizeMask64` (regmask_amd64.go 63, regmask_other.go 0xff) only masks shifts below 64. I expect the same output on amd64 and arm64; parity-arm64 settles it.
- Rust `{:.12}` against Go `FormatFloat('f', 12)`, and `parse::<f64>` against `isPDFReal` plus `ParseFloat` (Area A): unit tests with Go-generated vectors in WP 2 and WP 3.

**Go toolchain and partial reviews:**

- The Go oracle must run a pinned Go patch release (`golang:1.27.4`, recorded in docs/parity.md). A patch release could change the flate encoder.
- `bufio.Scanner`'s `ErrTooLong` boundary: WP 3 tests exactly-`max`-length lines.
- Not reviewed in detail:
  - types/utf16.go, date.go, pdfDocEncoding.go, model/version.go, input_limit.go `CheckInputSize`;
  - the body of `EnsureValidFreeList`;
  - `renderIndexed`, `renderDeviceN`, `renderICCBased` and `softMask`;
  - which parts of pdfcpu/annotation.go `Annotation` reaches.

  The WP owner traces each.

**Verified by reading (Area B), but not traced line by line:**

- every validate function's reachability;
- the per-file "reached" claims, which rest on the dispatch structure. The WP owners confirm them.

**Open decisions:**

- **X1 and X2 are in-crate** (section 11). Only the flush marks can need a small deflate-rs PR.
- **The Go oracle runs a patched snapshot.** That departs from the convention of a plain snapshot. It is justified above and listed hunk by hunk in docs/parity.md.
