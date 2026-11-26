# Differences from pdf-go

pdf-rs keeps pdf-go's behaviour where byblos can see it: the same output bytes, the same error
class, the same limit rules. The parity job ([parity.md](parity.md)) checks that on a large
corpus. This page lists where the Rust API or behaviour differs on purpose, and what is known not
to match yet.

## API

- **Bytes in, bytes out.** Go takes an `io.ReadSeeker` and writes to an `io.Writer`; pdf-rs takes
  `&[u8]` and returns `Vec<u8>`. There is no file I/O in the crate.
- **No `context.Context`.** Every long call takes `cancel: &AtomicBool` and returns
  `Error::Cancelled`. There are no `ProgressOptions`.
- **One config.** `Config { limits, cmd }` replaces `*model.Configuration`. The fixed Go settings
  (relaxed validation, no link validation, object and xref streams on write, optimize on,
  `DecodeAllStreams` off, empty passwords) are constants. Only `Cmd::{Validate, AddProperties,
  ListProperties}` exist.
- **Accessors.** Go returns the map stored in the table, so writes through it persist. Rust has
  `*_mut` accessors for those sites (`catalog_mut`, `dereference_dict_mut`, `entry_mut`).
  `dereference_stream_dict` returns an owned clone that shares the `Bytes`. `page_dict` returns
  copies; `InheritedPageAttrs::resources_owner` names the ancestor page tree node whose
  `/Resources` the page inherits (Go returns that node's map, xreftable.go:2164-2170), so a
  change to inherited resources goes through `dereference_dict_mut` on it.
- **Image.** `extract_image` returns `Option<Image { data, file_type }>`; `Some` always, because
  Go returns a nil image only together with an error.
- **Errors.** One `Error` enum with a predicate for each `errors.Is` of Go, and `Error::class()`
  for the stable token. A validation error does not keep the object number Go puts in the message.
  `Display` gives Go's `err.Error()` text for sentinels (`page not found`), untyped `fmt.Errorf`
  errors (`wrong type <<nil>>`, keeping each Go site's `%v` or `%T` verb) and
  `dereferenceDictTypeError` (`obj#3 gen#0: expected types.Dict, got types.StreamDict`); the
  other variants keep a `pdf: ` prefix.
- **MD5.** `pdf::crypto::md5(&[data])` is Go's `md5.Sum(data)` (for a document `/ID`). It takes
  no `Limits`, unlike the rest of the API: it allocates nothing and cannot fail.
- **Go panics.** A site where Go panics returns `Error::Panic`. The library is written not to panic on
  any input (see [safety.md](safety.md)).

## Limits and memory

- The `%v` text of an object in an error message (`wrong type <...>`) is cut after 64 KiB and ends in
  `...`; Go prints it whole. Shorter text is identical.
- Limit fields are unsigned. Go's negative `MaxDecodeBytes` ("unlimited") cannot be written; a
  value over `i64::MAX` is clamped to it.
- `max_alloc` and `max_threads` are new, and `Error::Alloc` and `Error::OutOfMemory` are Rust-only
  classes (`limit/alloc`, `oom`). Go never answers them. [limits.md](limits.md) has the rest.
- `max_recursion_depth` above 256 (`config::MAX_RECURSION_DEPTH_CEILING`, the parser's `MAX_NESTING`) acts as 256,
  because the native recursions (page tree, outlines, number trees, forms, optimize, write) share a fixed stack
  where Go's grows. The error stays `Sentinel::MaxRecursionDepth`. The defaults are unaffected.
- Go's cumulative decode budget counts slice capacity, not length. pdf-rs models Go's capacity
  (`src/gocap.rs`: `bytes.Buffer` growth, `roundupsize`) so the same stream trips the same limit.

## Time, randomness and order

- **Clock.** `WriteEnv` carries the time. `Default` reads the system clock in UTC; Go uses the
  local zone. There is no zone database. Parity pins the clock to 2000-01-01T00:00:00Z.
- **Randomness.** `WriteEnv` carries the random source: `/dev/urandom` by default (Unix only, no
  Windows), used for the AES initialisation vectors of the encrypted write fallback. Parity pins
  it to byte `i mod 256` per request.
- **Map order.** Go randomises map iteration, so some of its output (object order in the write
  fallback, resource dictionary order, file identifiers) varies from run to run. pdf-rs always
  visits `BTreeMap`s in sorted order, which is one execution Go allows. The oracle's Go side
  carries a patch that sorts those loops, and runs twice to prove it found them all
  ([parity.md](parity.md), Determinism).

## Ported inside the crate

Where Go's output is bit-exact, the Go algorithm is ported rather than replaced with a library
that would differ:

- the Flate encoder (Go's `compress/flate` levels 1, 6 (default) and 9, and `compress/zlib`), because PNG IDAT
  chunk boundaries and the encode parity follow Go's `Write` calls; deflate-rs only decodes;
- Go's `image/jpeg` decoder (entropy decode, IDCT and colour handling; jpeg-rs's libjpeg-style
  entropy decoder differs from Go on corrupt data, so jpeg-rs only supplies the image types, `Error::Jpeg` and the test encoder), the
  `encoding/gob` encoder and decoder subset for the CMYK path, the PNG writer, the TIFF writer
  (hhrutter/tiff), and the crypto primitives (MD5, RC4, AES, SHA-2).

DCT status: `src/filter/gojpeg.rs` ports Go 1.27 `image/jpeg` end to end (marker loop, Huffman
tables and decode, baseline and progressive scans, restarts, IDCT, colour conversion) and never
calls jpeg-rs to decode. CMYK and Adobe YCCK JPEGs, and Go's non-standard ("flex") sampling
factors, decode exactly as Go does, with Go's `FormatError`/`UnsupportedError` text and classes
`jpeg/format`/`jpeg/unsupported`; none of them is a known gap.

## Known gaps

- **CCITT.** `K > 0` is unsupported, with the same error class as Go.
- **Not reviewed in detail** (design.md section 16): the segmentation of the LZW, ASCII85 and
  CCITT readers for the capacity model, and the exact gob type-descriptor bytes. The parity `.cap`
  and golden outputs settle them.

## Not ported

Everything byblos never calls: the CLI, signing, forms, stamps and watermarks, n-up, booklets,
merging, page edits, bookmarks, attachments, fonts, creating or changing encryption, non-empty
passwords, configuration files. [scope.md](scope.md) lists them with the reason.
