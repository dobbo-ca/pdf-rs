# Limits

`pdf::Limits` mirrors pdf-go's `model.ResourceLimits` field for field (configuration.go:493-550,
defaults :573-591) and adds the two fields every port shares, `max_alloc` and `max_threads`. It is
carried by `pdf::Config { limits, cmd }`, which every entry point takes (`api::read`,
`read_and_validate`, `validate`, `read_validate_and_optimize`, `properties`) or reads from the
document (`write_context`, `extract_image`, `XRefTable::decode_stream`). There is no entry point
without limits. A document keeps its `Config` for its whole life (`ctx.xref.conf`), as Go does.

All byte and count fields are `u64`; `max_threads` is `usize`. Go's "negative means unlimited" for
`MaxDecodeBytes` cannot be written in `u64`; a value above `i64::MAX` is clamped to it, since Go
cannot express more.

## Fields

| Field | Go field | Default | Zero rule | Over it |
|---|---|---|---|---|
| `max_input_bytes` | MaxInputBytes | 0 | off | `Limit(InputBytes)` |
| `max_object_bytes` | MaxObjectBytes | 64 MiB | the default | `Limit(ObjectBytes)` if set away from the default. At the default, Go's error is the untyped `ObjectBufferLimit` and a relaxed read skips the object (read.go:2378-2391); `Error::is(Sentinel::ObjectBufferLimit)` is true for both |
| `max_stream_bytes` | MaxStreamBytes | 512 MiB | | untyped `Corrupt` (class `syntax`), as in Go (read.go:466-480): encoded bytes of one stream read without a length; not `is_limit_exceeded()` |
| `max_decode_bytes` | MaxDecodeBytes | 512 MiB | the default | `DecodeLimit`: decoded bytes of one stream |
| `max_total_decode_bytes` | MaxTotalDecodeBytes | 0 | `decode_allowance(max_decode_bytes)`: 1 GiB up to 512 MiB, else twice the limit, saturating | `Limit(DecodeBudget { objstm })`; also `is_decode_limit_exceeded()` |
| `max_image_pixels` | MaxImagePixels | 100 000 000 | | `ImageLimit { pixels: true }` |
| `max_image_bytes` | MaxImageBytes | 512 MiB | | `ImageLimit { pixels: false }`: raw bytes, and 4 bytes per pixel for the rendered buffer |
| `max_object_count` | MaxObjectCount | 10 000 000 | | `XRefObjectRange` (class `limit/xref-range`), or an untyped `Corrupt` `/Size` error; not `is_limit_exceeded()` |
| `max_object_stream_count` | MaxObjectStreamCount | 1 000 000 | | untyped `Corrupt` (read/objstm.rs:55): `/N` of one stream; not `is_limit_exceeded()` |
| `max_object_stream_total_count` | MaxObjectStreamTotalCount | 0 | off | `Limit(ObjectStreamEntries)`: entries over all object streams |
| `max_parsed_bytes` | MaxParsedBytes | 0 | off | `Limit(Parsed)`: Go's estimate, 64 per array element, 96 per dictionary entry, 256 per dictionary (xreftable.go:2198-2202) |
| `max_stream_total_bytes` | MaxStreamTotalBytes | 0 | off | `Limit(StreamTotal)`: encoded stream bytes read |
| `max_object_stream_first` | MaxObjectStreamFirst | 16 MiB | | untyped `Corrupt` (parse/mod.rs:1437): `/First` of one stream; not `is_limit_exceeded()` |
| `max_xref_entries` | MaxXRefEntries | 10 000 000 | | untyped `Corrupt` "xref entry count exceeds limit" (read/xref.rs:237); a subsection range over it is `XRefObjectRange`; not `is_limit_exceeded()` |
| `max_recursion_depth` | MaxRecursionDepth | 100 | the default | `Sentinel::MaxRecursionDepth` (recursion.go:108-123); values over 256 (`MAX_RECURSION_DEPTH_CEILING`) act as 256, see go-differences.md |
| `max_alloc` | (Rust only) | 4 GiB | a real cap | `Error::Alloc { got, max }` |
| `max_threads` | (Rust only) | `available_parallelism()`, read once | counts as 1 | none |

`Limits::decode_limit()`, `total_decode_limit()`, `object_bytes_limit()`, `recursion_depth()` and
`threads()` resolve the zero rules. The defaults are constants (`DEFAULT_MAX_*` in `pdf::config`).

Two budgets inside Go are fixed and cannot be configured: the object read budget,
`MaxObjectBytes + 8 x file size` (read.go:2801-2830), and the separate object-stream decode
allowance used when `max_total_decode_bytes` is 0 (xreftable.go:2310-2313). The fixed Go
configuration (relaxed validation, `WriteObjectStream`, `WriteXRefStream`, `Optimize`, empty
passwords) is constants, not fields.

## Errors

`pdf::Error` has one variant per Go error the port tells apart; the limit errors are distinct
variants so a caller can match on them. `Error::is_limit_exceeded()` mirrors
`errors.Is(err, model.ErrLimitExceeded)` (every `Limit(_)`), and `is_decode_limit_exceeded()`
mirrors `errors.Is(err, filter.ErrDecodeLimitExceeded)` (`DecodeLimit` and `DecodeBudget`).
`Error::root()` strips `Context` wraps. `Error::class()` is the short token the parity oracle
writes (`limit/decode`, `limit/total-decode`, `limit/objects`, `limit/alloc`, ...); the mapping is
in [parity.md](parity.md) and design.md section 5.

## `max_alloc`

The peak working memory of one document plus the call in progress, all threads together, nested
crates included. It is a ledger (`pdf::Ledger`, handle `pdf::Mem`): every buffer that scales with
the input is charged before it is allocated (`Vec::try_reserve_exact`, never `vec![0; n]` on an
unchecked `n`) and released when its last owner drops, so a long-lived `Context` does not leak
budget. The ledger belongs to the `XRefTable`; `ctx.xref.mem()` is its handle, and
`Mem::new(&limits)` makes a stand-alone one for a call without a document.

| Memory | Charged | Released |
|---|---|---|
| `Raw` and `Content` buffers, object-stream decode results | when the `Bytes` is made | when the last clone drops |
| Parsed objects | Go's estimate (`chargeParsed`: 64 per array element, 96 plus the key per dictionary entry, 256 per dictionary, the scalars), whatever `max_parsed_bytes` is, to a `Held` in the `XRefTable`; the ledger alone gets a dictionary entry's B-tree cost above 96 (2 x (24 + `size_of::<Object>()`) + 16 in all) | when the `XRefTable` drops |
| Cross-reference table entries | 2 x (16 + `size_of::<Entry>()`) per new key from an xref section or stream or the repair scan (the key, the entry, a compressed entry's object-stream number, B-tree nodes about half full), to the same `Held` | when the `XRefTable` drops |
| Object lists of xref streams | 8 bytes per object number, before the list is made, to the same `Held` | when the `XRefTable` drops |
| Optimizer page font and image sets | 64 bytes per access, to the same `Held` | when the `XRefTable` drops |
| The optimizer's other registries: caches, duplicate lists, the font registry and records, image records, `visited` | before each insert: `NR_BYTES` (48) per object number, names and records by size; `DuplicateFontObjs` and `DuplicateImageObjs` to the table's `Held`, `visited` to its page walk, the rest to the pass | when the pass (or the page walk) ends; the table's share when the `XRefTable` drops |
| Content parsing's resource names and `pre + rest` copies | 2 x 24 + 16 + the name per new name; each copy's length, before it is made | when the consolidation of the page ends |
| Copies of an encrypted string (unescaped, deciphered or enciphered, escaped) | 7 x its length + 192, while it is decrypted or encrypted | when the string is done |
| A decrypted string's growth (capacity over the parsed length) | to the table's `Held` | when the `XRefTable` drops |
| Validation's and the optimizer's copies of the page root, of `Kids` arrays and of dictionary keys | `Object::copy_bytes` (twice for `Kids` in validation, for the kept kids), before the copy | when the copy drops |
| The optimizer's inherited page resources (each page tree frame's copy, and the copy each page keeps) and validation's name tree (each node's dictionary and `/Names` copies) | `dict_copy_bytes` / `array_copy_bytes`; what a page or the tree keeps goes to the table's `Held` | when the frame ends, or when the `XRefTable` drops |
| Object buffers, filter intermediates, rasters, PNG and TIFF encoder state | per call | when the call ends |
| Returned image and written file bytes | while built | when handed back |

The `XRefTable`'s own charge only grows: an entry or object that is replaced or deleted is not
given back until the table drops.

Before a nested crate runs, pdf-rs charges its working memory and passes the charge as the crate's
`max_alloc`. deflate-rs: `min(left, 256 KiB)` (the reader's window), passed as its `max_alloc`; the output is
charged as it grows and a 32 KiB buffer is charged separately. deflate-rs is the only nested crate:
`gojpeg` (the in-crate `image/jpeg` port) charges its planes, progressive coefficients and the
RGBA/CMYK output on the document ledger before it allocates them.

The 4 GiB default is a backstop: it sits above the peak that Go's own limits allow at Go's
defaults (512 MiB of stream, 1 GiB of decode allowance, the input, and a render of 100 megapixels
at 4 bytes per pixel), so it does not change what Go would accept. A caller in a 2-CPU, 1 GiB pod
sets `max_threads = 2` and `max_alloc = 768 << 20`. The `gocap` module models Go's slice capacity
because `max_total_decode_bytes` is counted in it (`cap(sd.Content)`), not in lengths; see
design.md section 8.

### Shared ledger

A caller that holds several documents in one call (byblos's merge) passes one `Mem` to
`api::read_with_mem`, `api::read_and_validate_with_mem`, `api::read_validate_and_optimize_with_mem`
or `api::create_xref_table_with_root_dict_with_mem` (`XRefTable::new_with_mem` and
`Context::new_with_mem` below them). Every charge of each document, its `Held`, its working-copy
reserves and its nested crates, goes to that ledger, so the maximum of the caller's `Mem` is the
peak of the whole call. Each document's `Limits::max_alloc` is then not used; the other fields of
its `Limits` still apply to it alone. A document that does not fit in what the others left fails
with `Error::Alloc`. The entry points without `_with_mem` make a fresh `Mem::new(&limits)`, as
before.

### Working-copy reserve

Validation, the optimizer and the writer copy the object they are at (`resolve_entry`, the
`.cloned()` lookups, the model walks they call) instead of borrowing it, and drop the copy when
the frame returns. These copies are not charged one by one; each pass charges a reserve for them
instead. `XRefTable::start_reserve` (model/xref.rs:997) runs at the start of
`validate_xref_table` (validate/mod.rs:166), `optimize_xref_table` (optimize/mod.rs:416) and
`api::write_context` (api.rs:166). It scans the loaded objects once, without allocating, and
charges `WORKING_COPIES` (3) x M until the pass ends, where M is the largest copy path of one
object: `path_copy_bytes(o)` = `copy_bytes(o)` + the largest `path_copy_bytes` of its elements or
values. An object stream object that is parsed during the pass raises the reserve to cover it
(`grow_reserve`, model/deref.rs:334). A pass inside a pass keeps the outer reserve.

Why M is a path sum and not the largest `copy_bytes`: the frames of one object form a path into
it, and each frame copies the part it enters, which contains the parts the deeper frames copy.
A chain of direct dictionaries (an action's `/T` target chain, a `/Next` chain, nested function
or colour space arrays) with a large leaf has that leaf in every level's copy, so the copies on
the path add up to `path_copy_bytes`, up to the nesting depth times the leaf, not to one copy of
the object.

Why 3: the frames read for this audit hold at most two copies of the part they are at: the raw
entry (`d.find(..).cloned()`, kept for the error context and the reference number, as in
`validate_target_dict_entry_depth` and `validate_piece_info`) and the resolved copy that
`resolve_entry` returns (the typed `validate_*_entry` wrappers move it, they do not copy it
again). The third is for a frame that also keeps a working copy to change and store back (the
catalog in `validate_xref_table`). The optimizer compares two objects at a time (a font or image
against a registered one). This is a reading of the patterns, not a proof over all ~300 sites.

What it does not cover: the frames of *different* indirect objects on one path (catalog, page,
annotation, action, next action, ...). Each holds a copy of its own object, so a chain of n
distinct objects of size M holds up to 3 x n x M, while the reserve is 3 x M. Their sum is at most
3 times the objects on the path, which the table's parse charge already counts once, so the peak
can exceed `max_alloc` by up to 3 times the parse charge of the objects on the longest reference
chain a pass follows (bounded by `max_recursion_depth` per recursion family). Charging that
exactly needs per-frame `Held`s (see "Open").

## Cancellation

A call that can run long takes `cancel: &AtomicBool` (the `context.Context` of Go) and checks it
per object, per row or per stream, returning `Error::Cancelled`. `api::read` checks it before it
does anything else, as `pdfcpu.Read` does. The `dereference*` lookups, which have no cancel
parameter in Go either, use an internal flag that is never set; the work they can trigger (a lazy
object-stream decode) is bounded by `max_decode_bytes` and the ledger.

## Appendix: Input-sized allocations and their charge

This is an audit of the non-test code in `src/`. It covers every `.clone()`, `.cloned()`, `.to_vec()`, `.to_owned()`,
`collect`, `Vec::with_capacity`, `vec![..]`, `String::with_capacity`, `format!`, `extend`,
`extend_from_slice`, `push` and `insert` in a loop, and every sort or dedup scratch buffer. It
leaves out `src/read/stream.rs` and the `deep()` / `check_written_annotation` part of
`src/write/objects.rs`, which are audited separately. Line numbers are at the time of the audit.

Each row gets one decision:

* **a**: bounded by a constant or by a small fixed structure.
* **b**: charged already.
* **t**: a transient copy of one object (or one entry, scalar or key) that the table already
  charges. The copy is released before the walk moves on. At most one copy is live per frame of
  a walk that is bounded by `max_recursion_depth`. It is not charged; see "Open" below.
* **r**: a working copy made inside a validate, optimize or write pass, covered by the
  working-copy reserve of the pass (see "Working-copy reserve" above).
* **fixed**: charged by this audit.
* **open**: input-sized and not charged; see "Open" below.

| Site(s) | What | Decision |
|---|---|---|
| everywhere: `Error::*(format!(..))`, `.to_string()`, `.context(..)`, `String::from_utf8_lossy(..).into_owned()` (`lossy`) | error and context text. It holds at most one key, name or number of the object at fault | a (error path, one scalar) |
| parse/mod.rs:194 `try_vec`, :465 `hex_string`, :555 `clean_obj_prolog`, :771-772 array push, :794 dict insert | the parsed object | b: `chargeParsed` (64 per element, 96 + key per entry, the B-tree surplus, the scalars) to the table's `Held`. A scalar is charged right after it is parsed, and it is no longer than the object buffer, which is charged |
| parse/mod.rs:1314-1340 | object list of an xref stream | b: `charge_objs`, 8 per number |
| parse/mod.rs:1244, :1445 `sd.clone()` | typed copy of a just-parsed xref stream or object stream dictionary | t |
| parse/content.rs:330, :558-604 `prn.insert(.., name.to_vec())` | `PageResourceNames`: one entry per distinct resource name in a page's content | **fixed**: `Names::insert` (parse/content.rs:41-46) charges 2 x 24 + 16 + the name before a new name is copied, to the `Mem` that `parse_content` takes (the table's, model/resources.rs:590); the returned `Held` lives as long as the names. The glued-operator `pre + rest` re-copy (O(n^2) time, as in Go) is kept for parity, but `parse_content` takes `cancel` and checks it once per token, returning `Error::Cancelled` |
| parse/content.rs:641-643, :677 | Go's `pre + rest` copy of the rest of the content | **fixed**: each copy is charged before it is made (parse/content.rs:666) and released with the copy it replaces; `pre`'s capacity is charged as it grows (:704) |
| read/mod.rs:237, :311-331, :561-565; read/xref.rs:653-660; read/repair.rs:53, :72, :400-527 | line and object read buffers | b: `grow_buf_by` / the buffer's `Held` |
| read/xref.rs:70-78, :479-489 | backward scan blocks | a: one block |
| read/xref.rs:366, :413-418, :519 | trailer `Info` dictionary, `ID` and `AdditionalStreams` copies | b: within the trailer's parse charge, which stays in the table after the trailer drops |
| read/xref.rs:724 `fields.to_vec()` | the fields of one xref line | a |
| read/mod.rs:1050, :1073, :1082 (`dereferenced_object`) | the returned copy of a loaded object (callers read/repair.rs:133 and read/objstm.rs:350 drop it; read/deref.rs:480 is the encryption dictionary) | t |
| read/deref.rs:300; read/objstm.rs:72-95, :148 | table key list; object stream offsets and prolog | b (read/deref.rs:298) / a (prolog: one object stream's `/First`, bound by `max_object_stream_first`) |
| read/repair.rs:196, :469, :599-603 | repaired root dictionary; table inserts | b (repair.rs:192-196; `ENTRY_BYTES`) |
| model/deref.rs:291-306 | `Bytes` and `Result` clones of object stream content | a (shared `Arc`) |
| model/deref.rs:572, :593, :624, :830; model/equal.rs:110, :227; model/pages.rs:182, :187, :223, :298 | copies of a name, a string, a `/Type`, a box or an object being compared | r (model/xref.rs:997, in a pass; t outside one) |
| model/equal.rs:96, :177 | the set of object pairs an equality compare has visited | **fixed**: `PAIR_BYTES` per insert, to the compare's own `Held` (model/equal.rs:104, :186), released when it returns |
| model/pages.rs:148 (`page_node`), :213 | a copy of each page tree node without `/Kids` on the walk; the `/Resources` in effect | r (model/xref.rs:997, in a pass; t outside one) |
| model/resources.rs:611-615 (`consolidate_node`) | the inherited resources that each page tree frame copies (`attrs.clone()`) | **fixed**: charged before the clone; what the node's own `/Resources` add is charged right after the merge |
| model/resources.rs:625-633 | the consolidated resources that each page stores | **fixed**: the frame's charge moves to the table's `Held` |
| model/resources.rs:257, :277, :298, :384, :462-510, :535-539, :708 | content and resource lookups; the merge of one node's resources; the cut of one sub dictionary; `DeleteObject`'s walk copy | r (model/xref.rs:997; the merge is also charged with its frame, see above) |
| model/resources.rs:357 | combined page content | b |
| model/names.rs:95-120, :198-223 | name tree entries; the tree written back as dictionaries | b (entries: the 4 times the array of validate/name_tree.rs:1077) / **fixed** (write-back, to the table's `Held`: the new `/Names` object, `ENTRY_BYTES` (model/names.rs:142); the hex `/Limits` (:200); the hex keys and value copies of a leaf's `/Names` (:213); `ENTRY_BYTES` and the dictionary of each new kid object (:228); the root copy registered in `/Names` (:243)) |
| model/xref.rs:767, :784 | stream dictionary copies returned to the decoder | r (model/xref.rs:997, in a pass; t outside one); buffers shared |
| model/xref.rs:428, :465, :481, :955 | table inserts | b (`ENTRY_BYTES`, and the object's parse charge) |
| validate/name_tree.rs:1059 | the `/Names` value copy (a direct array) | **fixed**: charged before the clone |
| validate/name_tree.rs:1070 | the working copy of the `/Names` array and the copy that is stored back | **fixed**: twice the array, before the clone |
| validate/name_tree.rs:1077 (and :1081 `nd.set`, :1041 `append_to_names`) | the array copy the node keeps, its entries (decoded and raw key, value) and their `Vec` | **fixed**: 4 times the array, to the table's `Held`, since the tree lives in `x.names` |
| validate/name_tree.rs:1215 `Node::new(d.clone())` | the node's dictionary, kept in `x.names` | **fixed**: to the table's `Held` |
| validate/object.rs:96 (`resolve_entry`, behind every `validate_*_entry`), :969, :1033, :1106, :1125, and the `dereference_*(..)?.cloned()` / `d.find(..).cloned()` copies: name_tree.rs 38 more sites, form.rs 34, action.rs 23, struct_tree.rs 21, page.rs 21, mod.rs 21, font.rs 16, file_spec.rs 16, annotation.rs 16, xobject.rs 11, media.rs 11, colorspace.rs 10, shading.rs 9, outline.rs 9, optional_content.rs 8, extgstate.rs 8, destination.rs 7, xmp.rs 6, three_d.rs 6, thread.rs 5, number_tree.rs 4, metadata.rs 4, pattern.rs 3, property.rs 2, info.rs 1, function.rs 1 | validation's per-frame copies of the object or entry being validated | r (validate/mod.rs:166), except validate/page.rs:1459, :1790-1808 (the page root and `Kids`), which are b as well |
| validate/annotation.rs:2156, :2169, :2182, :2250-2270 | the annotation copy being validated; the `Annotation` records kept in `vstate.page_annots` | r (validate/mod.rs:166) / **fixed**: per entry, its record, its `ind_refs` slot and its slot in the cleaned array the page stores, each `Vec` at up to twice its count (`ANNOT_RECORD_BYTES`), plus the dictionary of a direct annotation, to the table's `Held` (validate/annotation.rs:2265) |
| validate/form.rs:364, :388; validate/media.rs:1418-1440; validate/mod.rs:243, :497; validate/info.rs:81; validate/metadata.rs:123-157 | signature cache; rendition reference sets; catalog XMP; the named destinations dictionary (`vstate.dests`); info keys; XMP creator and producer | **fixed**, to the table's `Held` unless noted: `SIG_BYTES` per new signature (validate/form.rs:395); 2 references per `Annots` element to the pass's `Validator::held`, released when the pass ends (validate/media.rs:1432); the parse of the XMP (`parse_xmp`, validate/xmp.rs: the element stack, the name space map, the token's names, attributes and text, the list entries and strings, all charged before they are built, released when the parse returns, and the cancel flag checked at every token), then `XmpMeta::copy_bytes` (validate/mod.rs:256); `dict_copy_bytes` of `/Dests` (validate/mod.rs:513); key (3 bytes per byte, for `lossy`), value and entry of each property (validate/info.rs:83); the working copy of the XMP for the call (validate/metadata.rs:132), and the joined title, author and subject, creator, producer and keyword list (:133) |
| optimize/frame.rs:64-75 | the path of a walk location | a (bound by the depth) |
| optimize/frame.rs:207-225; optimize/mod.rs:409-416, :526, :535, :1552-1567 | dictionary key lists; page font and image sets; `Kids` copies | b |
| optimize/mod.rs:307, :481, :489, :650, :730, :838; optimize/font_name.rs:80-93 | the font or image dictionary being compared or registered | r (optimize/mod.rs:416) |
| optimize/mod.rs:249, :276-278, :321, :763; optimize/font_name.rs:60-61 | `FontObject` names and the font registry | **fixed**: before each insert, the `Fonts` key and number (optimize/mod.rs:319), the record by `font_object_bytes` (:396 for the form, :869), a duplicate's resource name (:781), to the pass's `Held` (the form's to a `Held` of `cache_form_fonts`) |
| optimize/mod.rs:202-204, :570, :682, :710, :772, :813-925, :936-997, :1585-1603, :1660-1661, :1706 | sets and lists of object numbers (caches, duplicates, `corrupt_font_res_dicts` locations) | **fixed**: `NR_BYTES` (48) before each insert: `Cache` (:669, :1604, given back when it is cleared, :1683), `visited` (:261, per page walk), `corrupt_font_res_dicts` (:815, `loc_bytes`), images and hashes (:927, :938, :982, :997, :1039), `FormStreamCache` (:1061, :1090, :1114), `FormResourceCache` (:1122), to the pass's `Held`; `DuplicateFontObjs` and `DuplicateImageObjs` (:1731, :1757, :1864) to the table's. The working lists are charged before they are made (:1078, :1815) or no longer copied (the `Fonts`, hash, `DuplicateFonts`, `DuplicateImages` and `corrupt_font_res_dicts` loops read in place) |
| optimize/mod.rs:831 | SHA-256 of an image's raw bytes | a (the digest; the input is borrowed) |
| write/mod.rs:133-136, :191, :783-845 | output buffer, xref stream buffer | b |
| write/mod.rs:332-344; write/pages.rs:158-162, :213-225 | root, page and page node copies | b |
| object/write.rs `dict_indented`, `array_indented`; model/deref.rs `go_v`; crypto/mod.rs `file_id` | the indented `String()` text of an object (error `%v` text, file ID hash) | **fixed**: streamed into a `Sink`, no child text is copied into its parent; `go_v` keeps at most `GO_V_CAP` (64 KiB) and stops writing there; `file_id` streams into the MD5 hasher (no buffer) |
| write/mod.rs:438-449 | the info dictionary values for the file ID | r (api.rs:166) |
| write/mod.rs:649-653 | the list of table keys | **fixed**: 8 per table entry, before the list is made (write/mod.rs:649), released with it |
| write/mod.rs:465-470, :704-714, :867-877, :881-947 | file ID, trailer, xref stream dictionary | a (the ID is 2 MD5 digests; the dictionaries hold references and integers) |
| write/pdf20.rs:90-96 | the table key list of the PDF 2.0 font pass | **fixed**: 8 per object |
| write/pdf20.rs:115-122 | the working copy of every dictionary, array and stream in the PDF 2.0 font pass | **fixed**: `copy_bytes`, before the clone |
| write/pdf20.rs:35; write/corefont.rs:103, :112, :128 | `/Type`, `/Encoding` and `/Differences` copies | r (api.rs:166) |
| write/corefont.rs:168-186 | glyph names and widths of a standard font | a (256) |
| image/extract.rs:71-77, :186, :214, :283, :349, :382; image/render.rs:167, :241, :314, :823, :982, :1062 | filter names, parameter, colour space and decode copies | t |
| image/extract.rs:135; image/render.rs:244, :253 | `Bytes` clones | a (shared) |
| image/render.rs:180 | decode ranges | a (one per component, at most 32) |
| image/mod.rs:62-93; image/png.rs:114-148, :326-347; image/tiff.rs:229-235; image/render.rs:432 | encoder and raster buffers | b |
| image/tiff.rs:95-213 | IFD entries | a |
| filter/mod.rs:120-194, :272-273; filter/flate.rs:163-164, :567-568, :611, :632, :688-694; filter/ccitt.rs:272-278, :559; filter/dct.rs:57; filter/run_length.rs:24, :95; filter/gojpeg.rs:95-96, :852-861, :954-957, :1614 | decode output and row, plane and coefficient buffers | b |
| filter/ascii85.rs:92-94; filter/lzw.rs:70-72; filter/ccitt.rs:31-41; filter/goflate/* | fixed-size buffers, code tables, compressor state | a (goflate: b, `writer_bytes` in zlib.rs:95) |
| filter/gob.rs:138-300, :356-372, :576-766 | gob message headers and type plans; the message parts borrow the image | a (`Message::to_vec` is used only by tests) |
| filter/flate.rs:104 | an error value | a |
| crypto/auth.rs:60-122, :153-217, :271, :334; crypto/sha2.rs:132-139; crypto/mod.rs:121 | keys, digests, password hashes (`hash_rev6` repeats at most 64 x (127 + 64) bytes) | a |
| crypto/mod.rs:142-146, :387-437, :498-504, :622-628 | string and stream ciphers | b |
| crypto/validate.rs:376, :425, :600, :650 | encryption and crypt filter dictionary copies | t |
| object/array.rs, object/dict.rs, object/date.rs, object/text.rs, object/string.rs | constructors and conversions that build the value the caller asked for | the caller's row |
| doc/info.rs, doc/property.rs, doc/create.rs | info and property values the caller passes in | a (caller input, not file input) |

### Open

* **r (per-frame copies in validation, the optimizer and the writer).** Covered by the
  working-copy reserve of each pass (see "Working-copy reserve"), up to the chain of distinct
  indirect objects that it leaves out. Closing that remainder means returning a `Held` with each
  `validate_*` result and changing its roughly 300 callers.
* **t (outside a pass).** The read's typed stream dictionary copies (parse/mod.rs), the returned
  copy of `dereferenced_object`, the encryption dictionary copies (crypto/validate.rs), the image
  entry copies (image/*, per `extract_image` call, which takes no reserve because the scan would
  run once per image) and the model lookups called outside a pass are one copy of one object,
  still not charged.
