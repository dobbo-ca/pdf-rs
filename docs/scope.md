# pdf-rs scope

pdf-rs is a Rust port of pdf-go (`github.com/dobbo-ca/pdf-go` at `113d693`). It covers only the
parts that byblos (at `086b88b`) reaches. byblos calls pdf-go only from `internal/pdfdoc`. This doc
lists those calls, the pdf-go code they reach, the configuration that picks the code paths, the
dependencies, and what is left out.

Paths: byblos paths are relative to `byblos/internal/pdfdoc/`. pdf-go paths are relative to
`pdf-go/` (`pkg/...`, `internal/...`). Line counts leave out test files.

How this was built: several people traced the call graph by reading the code. No call-graph tool
was used. I re-checked a sample of the `file:line` references (the entry points, the key model and
types methods, the config defaults and the third-party imports) against both checkouts. The
corrections from that check are listed in section 7.

## 1. Summary

pdf-go packages in scope:

| pdf-go package | Total non-test lines | Part in scope | Reason it is in scope |
|---|---|---|---|
| `pkg/api` | 16,147 | about 400 (api.go, validate.go, property.go) | thin entry points |
| `pkg/pdfcpu` (root) | 31,036 | about 13,000 | read.go, crypto.go, crypto_password.go, optimize.go, write*.go, extract.go, writeImage.go, image.go, property.go, info.go, createContext.go, object_frame.go, guard.go, errors.go |
| `pkg/pdfcpu/model` | 23,172 | about 5,500 | XRefTable/Context, parse.go, dereference.go, parseContent.go, equal.go, recursion.go, configuration.go (limits and defaults only) |
| `pkg/pdfcpu/types` | 3,852 | about 2,700 | object model |
| `pkg/pdfcpu/validate` | 27,758 | about 30,000 with the api layer; about 27,000 of the package itself | relaxed-mode validation |
| `pkg/filter` + `internal/filter/lzw` | 1,561 + 510 | about 1,300 decode, plus Flate encode | stream filters |
| `pkg/pdfcpu/font` | 1,886 | about 40 (`Name`, `ErrCorruptFontDict`) | used by optimize and validate |
| small support packages | — | about 400 | `fault`, `scan`, `safemath`, `internal/contextutil`, `internal/streamlimit`, `pkg/log` (as no-op guards) |

Approximate Go lines in scope:

- **About 25,000 without `validate`.** That is read, decrypt, page tree, decode, optimize, the
  write fallback, image extraction and properties.
- **About 55,000 with `validate`.**
- The validator is the biggest question in this scope. See section 4.4 and the open questions in
  section 8.

## 2. byblos entry points

Every pdf-go symbol that byblos code calls, apart from plain field and map access on `types`.
The "pdf-go definition" column is the line where the function is defined.

### 2.1 `pkg/api`

| Symbol | byblos evidence | pdf-go definition |
|---|---|---|
| `api.ReadContext` | pdfdoc.go:457 (Open), fontcensus.go:113, danglingrefs.go:38 | pkg/api/api.go:81 |
| `api.ReadContextFile` | fontcensus.go:99 (only caller is `FontDicts`, used by `cmd/byblos-fonts`) | pkg/api/api.go:93 |
| `api.Validate` | write.go:299 (4th arg is `*ProgressOptions`, passed as nil) | pkg/api/validate.go:118 |
| `api.ReadValidateAndOptimize` | optimize.go:30, properties.go:38 | pkg/api/api.go:304 |
| `api.Properties` | properties.go:73 | pkg/api/property.go:33 |
| `api.WriteContext` | deterministic.go:77 (fallback for encrypted or rootless tables) | pkg/api/api.go:169 |

### 2.2 `pkg/pdfcpu`

| Symbol | byblos evidence | pdf-go definition |
|---|---|---|
| `CreateXRefTableWithRootDict` | buildpages.go:305 | pkg/pdfcpu/createContext.go:25 |
| `CreateContext` | buildpages.go:385 | pkg/pdfcpu/createContext.go:87 |
| `ExtractImage(c, ctx, sd, thumb=false, resourceID, objNr, stub=false)` | pdfdoc.go:1426 | pkg/pdfcpu/extract.go:511 |
| `ColorSpaceComponents` | pdfdoc.go:1366, 1502 | pkg/pdfcpu/extract.go:273 |
| `PropertiesAdd` | properties.go:44 | pkg/pdfcpu/property.go:30 |
| `ErrWrongPassword` | encryptinfo.go:39 | pkg/pdfcpu/read.go:81 |

### 2.3 `pkg/pdfcpu/model`

| Symbol | byblos evidence | pdf-go definition |
|---|---|---|
| `NewDefaultConfiguration` | pdfdoc.go:82-83 (`defaultConfig`, pdfdoc.go:81) | model/configuration.go:942 |
| `Configuration.Limits.*` (written) | pdfdoc.go:86-91, 447-456, 1417-1420 | model/configuration.go (`ResourceLimits`) |
| `Configuration.Cmd = ADDPROPERTIES` | properties.go:37 | model/configuration.go (`CommandMode`) |
| `Context`, `ReadContext.UsingObjectStreams` | pdfdoc.go:318, fontcensus.go:175-176 | model/context.go:44, :203 |
| `XRefTable` (struct) | pdfdoc.go:516, text.go:374 | model/xreftable.go:128 |
| `XRefTableEntry.{Object,Free}` | buildpages.go:399/846, linearize.go:284, selfwrite.go:223, write.go:258 | model/xreftable.go:65 |
| `XRefTable.{Table,Root,Info,ID,Encrypt,E,PageCount,Size}` | linearize.go:75-116, 544; buildpages.go:249, 369; encryptinfo.go:24-29; properties.go:43, 52 | model/xreftable.go:128 |
| `Enc.{P,V,R}` | encryptinfo.go:29 | model/xreftable.go:98 |
| `EnsurePageCount` | pdfdoc.go:468, fontcensus.go:161 | model/xreftable.go:3002 |
| `Pages` | pdfdoc.go:477, buildpages.go:620, linearize.go:119 | model/xreftable.go:1333 |
| `Catalog` | buildpages.go:364, catalog.go:16, linearize.go:110 | model/xreftable.go:1264 |
| `PageDict(ctx, n, false)` | pdfdoc.go:582, annots.go:48, text.go:133/191/295/413, straighten.go:96 | model/xreftable.go:2721 |
| `InheritedPageAttrs` | pdfdoc.go:590-635, straighten.go:100-105 | model/xreftable.go:2009 |
| `Dereference` | pdfdoc.go:695, 723, 1616, 1659; linearize.go:287, 388, 532; danglingrefs.go:54 | model/dereference.go:129 |
| `DereferenceDict` | pdfdoc.go:525, 1071; text.go:144, 153; deterministic.go:86 | model/dereference.go:492 |
| `DereferenceArray` | pdfdoc.go:533, linearize.go:253 | model/dereference.go:477 |
| `DereferenceStreamDict` | pdfdoc.go:966, 1264, 1371, 1473, 1568 | model/xreftable.go:1205 |
| `InsertObject` | buildpages.go:311, 333, 405, 523, 718, 738 | model/xreftable.go:491 |
| `IndRefForNewObject` | text.go:74, 99, 113, 241, 356; deterministic.go:143 | model/xreftable.go:504 |
| `NewStreamDictForBuf` | text.go:62, 234, 349 | model/xreftable.go:515 |
| `FindTableEntry` | buildpages.go:395, 842; write.go:227 | model/xreftable.go:400 |
| `FindTableEntryForIndRef` | text.go:381, 508 | model/xreftable.go:408 |
| `VersionString` | linearize.go:100, deterministic.go:234, selfwrite.go:106 | model/xreftable.go:266 |
| `ErrLimitExceeded` | pdfdoc.go:107, 109 | model/configuration.go:562 |
| `ErrParsedLimitExceeded`, `ErrObjectBytesExceeded`, `ErrStreamTotalLimitExceeded` | pdfdoc.go:100-104 | model/xreftable.go:2205, 2209, 2212 |
| `Image` (`Reader`, `FileType`) | pdfdoc.go:1435-1442 | model/image.go:46 |

### 2.4 `pkg/pdfcpu/types`

| Symbol | byblos evidence | pdf-go definition |
|---|---|---|
| `Object`, `Dict`, `Array`, `Name`, `Integer`, `Float`, `Boolean`, `StringLiteral`, `HexLiteral` | throughout (see the inputs); for example pdfdoc.go:1621-1625, linearize.go:494-495 | types/types.go:69ff, types/dict.go:29, types/array.go:28 |
| `IndirectRef` (value form), `NewIndirectRef` (avoided, comment buildpages.go:758-761) | buildpages.go:851, linearize.go:444, deterministic.go:329 | types/types.go:554, :560 |
| `Dict.Find`, `NameEntry`, `IntEntry`, `Subtype`, `InsertInt`, `PDFString` | pdfdoc.go:529, 1226, 970; fontcensus.go:200, 230; text.go:70; linearize.go:474 | types/dict.go:95, 194, 211, 377, 68, 493 |
| `NewDict` | text.go:149, deterministic.go:142 | types/dict.go:32 |
| `Rectangle`, `RectForDim` | pdfdoc.go:615, 1630; straighten.go:110 | types/types.go:177, 213 |
| `StreamDict` fields (`Dict`, `Raw`, `Content`, `FilterPipeline`, `StreamLength`, `StreamLengthObjNr`) | pdfdoc.go:827-864, write.go:232-258, buildpages.go:816-827 | types/streamdict.go:37 |
| `PDFFilter{Name, DecodeParms}` | pdfdoc.go:834, write.go:255 | types/streamdict.go:31 |
| `StreamDict.DecodeWithLimit` | limits.go:209; pdfdoc.go:836, 861, 1268, 1372, 1410, 1509 | types/streamdict.go:402 |
| `StreamDict.Encode` | text.go:71, 238, 353 | types/streamdict.go:289 |
| `ObjectStreamDict`, `XRefStreamDict` (type-switch exclusion only) | linearize.go:294-297, selfwrite.go:228 | types/streamdict.go:231, 570 |
| `NewHexLiteral` | write.go:125 | types/types.go:522 |
| `HexLiteral.Bytes`, `StringLiteral.Value` | linearize.go:547, 551 | types/types.go |
| `StringLiteralToString`, `HexLiteralToString` | deterministic.go:100, 104 | types/utf16.go:158, 167 |
| `EscapedUTF16String` | buildpages.go:453 | types/utf16.go:131 |
| `DateTime(s, relaxed=true)`, `DateString` | deterministic.go:117, 118 | types/date.go:360, 27 |

### 2.5 `pkg/filter`

| Symbol | byblos evidence | pdf-go definition |
|---|---|---|
| `ErrDecodeLimitExceeded` | pdfdoc.go:107, 111, 758, 852, 1373, 1410, 1428; limits.go:212 | pkg/filter/filter.go:46 |
| `Flate`, `DCT`, `JPX`, `JBIG2`, `CCITTFax` (name constants) | pdfdoc.go:842, 1363, 1482, 1499 | pkg/filter/filter.go:34-42 |

## 3. Transitive closure, by pdf-go package and file

Only the reached functions are named. Section 6 lists the code left out of the same files.

### 3.1 Read path (`ReadContext` → `pdfcpu.Read`)

- **pkg/api/api.go**: `ReadContext` :81 (`fault.Catch`, `contextutil.Check`, `pdfcpu.Read`).
- **pkg/pdfcpu/read.go** (4,515 lines). Nearly all of it is reached; only `ReadFile` :140 is dead.
  - `Read` :164
  - Xref table: `readXRefTable` :2235, `offsetLastXRefSection` :267, `buildXRefTableStartingAt` :2163, `parseXRefSection` :1576, `parseXRefTableSubSection` :486, `xRefTableObjectNumber` :475 (enforces `MaxXRefEntries` and the `MaxObjectCount` backstop)
  - Trailer: `processTrailer` :1535, `parseTrailerDict` :1394, `offsetPrev` :1353, `scanForPreviousXref` :1282
  - Hybrid files and xref streams: `parseHybridXRefStream` :1070, `parseXRefStream` :1015, `extractXRefTableEntriesFromXRefStream` :829
  - Relaxed repair: `repairCorruptXRefSection` :2155, `bypassXrefSection` :1970, `newRepairScanner` :2369
  - Object reading: `object` :2849, `bufferedObject` :2832, `resolveObject` :2933, `dereferencedObject` :2991
  - Filter pipeline: `pdfFilterPipeline` :2651, `buildFilterPipeline` :2564
  - Stream content: `loadEncodedStreamContent` :3346, `readStreamContent` :3221, `streamLimit` :3310, `decodeStream` :3299 (decode is gated by `DecodeAllStreams`, which is false, so ordinary streams are only decrypted)
  - Object streams: `decodeObjectStreams` :3784, `parseObjectStreamWithBudget` :673, `decompressXRefTableEntry` :3554 (lazy objects)
  - Dereference loop: `dereferenceXRefTable` :4311, `dereferenceObjects` :4210, `unrecoverableObjectReadError` :3922
  - Linearization and hint-stream checks: :3829, :4187
  - Version: `identifyRootVersion` :4246, `validateInputVersion` :4358
  - Encryption: `checkForEncryption` :4467, `setupEncryptionKey` :4411, `handleUnencryptedFile` :4366
- **pkg/pdfcpu/crypto.go**, decrypt side (about 1,900 of 2,569 lines):
  - Encrypt dict validation: `supportedEncryption` :1687, `validateEncryptionVersion` :1310, `validateCryptFilters` :1486, `streamCryptMethod` :1816
  - Password checks with an empty password: `validateOwnerPassword` :693, then `validateUserPassword` :271. R2-R4 use `key` :313, `o` :350, `u` :396. R5 and R6 use `validate*AES256*` :491-650 and `hashRev6` :554. `validatePermissions` :822.
  - Decrypt: `decryptObjectStrings` :1780, `decryptDeepObject` :2176, `decryptStream` :2244, `decryptKey` :1862, `decryptAESBytes` :2315, `applyRC4Bytes` :2261
- **pkg/pdfcpu/crypto_password.go**: all of it (`preparePassword`, `processInput` :126). R5 and R6 call it even with "".
- **pkg/pdfcpu/model**:
  - context.go: `NewContext` :57, `ReadContext` :203
  - xreftable.go:
    - Limit charges: `ChargeObjectStreamEntries` :2184, `ChargeStreamBytes` :2216, `chargeParsed` :2240
    - Decode budget: `DecodeStream` :2273, `decodeStream` :2295, `DecodeImageStream` :2284
    - Table access: `Find` :354, `FindTableEntry` :400, `InsertNew` :427, `InsertAndUseRecycled` :435, `InsertObject` :491, `IndRefForNewObject` :504, `NewStreamDictForBuf` :515, `DereferenceStreamDict` :1205, `Catalog` :1264, `Pages` :1333, `EnsurePageCount` :3002
    - Page tree: `PageDict` :2721 and its walk (`processPageTreeForPageDictDepth` :2646, `checkInheritedPageAttrs` :2110, `rect` :2016, `pageObjType` :2555)
  - parse.go (1,645 lines, nearly all): `ParseObject` :1134, `parseObject` :1038, the value parsers, `ParseXRefStreamDict*WithLimits` :1302/:1307, `ObjectStreamDictWithLimits` :1350
  - dereference.go: `indRefToObject` :99 (the only edge from `Dereference` into object-stream decode and the parser), `Dereference` :129, `DereferenceArray` :477, `DereferenceDict` :492, `DereferenceIntegerEntry` :247, `DereferenceNumber` :262, `DereferenceNameEntry` :335, `ProcessRefCountsWithError` :90
  - recursion.go: `CheckRecursionDepth` :113/:118, `PageTreeVisit` :137-170
  - configuration.go: `DecodeLimit` :554, `TotalDecodeLimit` :565, `DefaultResourceLimits` :573
  - Small helpers: version.go, validation_error.go:43, message.go:50/55
- **pkg/pdfcpu/types**:
  - types.go: scalar types, `IndirectRef` :554, `Rectangle` :177, `NewRectangle` :183, `RectForDim` :213
  - dict.go: `Find` :95, typed entry accessors :160-300, `PDFString` :493 (silently drops unknown types such as `*IndirectRef` and `StreamDict`)
  - array.go: `PDFString` :150
  - streamdict.go:
    - Decode: `DecodeWithLimit` :402, `decodeLength` :407, `DecodeLengthWithLimit` :489, `preserveRawContent` :514, `parmsForFilter` :249, `fixParms` :357
    - `Encode` :289
    - Lazy object-stream objects: `LazyObjectStreamObject` :112 with `GetData`/`DecodedObject`
  - string.go: `Escape`/`Unescape`, `EncodeName` :261, `DecodeName` :285
  - utf16.go and pdfDocEncoding.go
  - date.go: `DateTime` :360, `DateString` :27
- **pkg/filter**: `NewFilter` :67, then decode for Flate (with PNG/TIFF predictors and `paeth.go`), LZW (`internal/filter/lzw/reader.go`), ASCII85, ASCIIHex, RunLength, CCITTFax (`ccittDecode.go`, Group 3 1-D and Group 4 only; K>0 is an error) and DCT (`dctDecode.go`, used only when `CSComponents==4`). JBIG2 and JPX return `ErrUnsupportedFilter` (:43), or pass through when they are the last filter. `flate.Encode` is reached from `StreamDict.Encode`.

### 3.2 Validation (`ValidateContext` → `validate.XRefTable`)

Reached from `api.Validate`, `ReadValidateAndOptimize` (Optimize, WriteProperties, Properties) and
`ReadContextFile` (FontDicts only).

- pkg/api/api.go: `readAndValidate` :236, `ValidateContext` :117, `operationConfiguration` :70.
  pkg/api/validate.go: `Validate` :118.
- pkg/pdfcpu/validate/xReftable.go:
  - `XRefTable` :51, `validateXRefTable` :91, `validateRootObject` :1876, `validateRootEntries` :1853
  - All 29 root-entry validators are reachable, because the dispatch table does not depend on mode.
  - `checkForBrokenLinks` :1751 returns early, because `ValidateLinks=false` (model/configuration.go:884).
- The other files in the package are all reached in relaxed mode: object.go, page.go (`validatePages` :1658), annotation.go, action.go, font.go, nameTree.go, numberTree.go, media*.go, extGState.go, xObject.go, form*.go, colorspace.go, structTree.go, optionalContent.go, fileSpec.go, function.go, outlineTree.go, threeD.go, shading*.go, thread.go, viewerPreferences.go, info.go, destination.go, pattern.go, property.go, image_arrays.go, metaData.go (`encoding/xml`), array_cardinality.go, active_content.go, rendition.go, tree_traversal.go, graph_depth.go, trapnet_states.go.
- Not reached: the link-check files (link_http.go, link_collect.go, destination_remote.go), and strict-only branches.
- Calls out of the package: `pdfcpu.Annotation` (pkg/pdfcpu/annotation.go:323) and `font.ErrCorruptFontDict`.

### 3.3 Optimize (`OptimizeContext` → `pdfcpu.OptimizeXRefTable`)

Reached from `ReadValidateAndOptimize`, because `Optimize=true` (configuration.go:893).

- pkg/api/api.go: `OptimizeContext` :145, `cmdAssumingOptimization` :290. `pdfcpu.CacheFormFonts` (optimize.go:1704) is called by `ReadValidateAndOptimize` afterwards.
- pkg/pdfcpu/optimize.go: `OptimizeXRefTable` :1821 runs these phases:
  - `fixReferencesToFreeObjects` :1691
  - `optimizeResourceDicts` :1759 (with `Cmd=VALIDATE`; skipped for `ADDPROPERTIES`, per `shouldOptimizeResourceDicts` :1812)
  - `optimizeFontAndImages` :1311
  - `ensureDirectWidthForXObjs` :1778
  - deleting the root /PieceInfo
- pkg/pdfcpu/object_frame.go (156 lines).
- model/xreftable.go: `ConsolidatePageResources` :2858, `PageContent` :2372 (decodes content streams), `DeleteDictEntry`/`DeleteObject`/`FreeObject`.
- model/parseContent.go: all of it (`parseContent` :597; `skipTJ` :115 returns an error, it does not panic).
- model/equal.go: all of it (`EqualObjects`).
- model/resource.go: `PageResourceNames`.
- font/fontDict.go: `Name`.

### 3.4 Write fallback (`api.WriteContext` → `pdfcpu.WriteContext`)

This runs only when `deterministicBytes` refuses: when `Encrypt != nil` or `Root == nil`
(deterministic.go:174).

- pkg/pdfcpu/write.go:
  - `WriteContext` :107, `writeContext` :117
  - `prepareContextForWriting` :221, `ensureFileID` :253 (not deterministic), `handleEncryption` :1160 (the RC4/classic-xref downgrade)
  - `writeXRef` :1197, which picks the xref stream or the classic table
- pkg/pdfcpu/writeObjects.go (about 980 lines): `writeStream` :507 (errors if `len(Raw) != *StreamLength`), `writeStreamDictObject` :576 (encrypts per object when `EncKey != nil`).
- pkg/pdfcpu/writeObjects_pdf.go, writePages.go, writeAnnotation.go (PDF 2.0 only), writeFont.go (PDF 2.0 only).
- pkg/pdfcpu/info.go: `ensureInfoDict` :110 (`time.Now()`).
- crypto.go encrypt side:
  - `encryptDeepObject` :2081, `encryptStream` :2226
  - `encryptAESBytes` :2279, which takes its IV from `crypto/rand`
  - `fileID` :2346
  - Encryption reuses the existing `EncKey`. No new encryption dictionary is built.

### 3.5 Image extraction (`pdfcpu.ExtractImage`)

- pkg/pdfcpu/extract.go:
  - `ExtractImage` :511, `prepareExtractImage` :418, `decodeImage` :441, `img` :477
  - `ColorSpaceComponents` :273 and its helpers :128-248
- pkg/pdfcpu/image.go: `imageBooleanEntry` :34, `integerEntryValue` :302, `prepareImageDecode` :349.
- pkg/pdfcpu/writeImage.go (about 1,150 of 1,199 lines):
  - Dispatch: `RenderImage` :1158, `renderImage` :1042, `pdfImage` :152, `softMask` :287
  - Indexed: `renderIndexed*`
  - DeviceN: `renderDeviceN`
  - CMYK: `renderDCTToPNG` :1148 (a CMYK JPEG is decoded and written out as PNG), `renderDeviceCMYKToTIFF` (`tiff.Encode` at :428 and :780)
  - Output is PNG or TIFF.
- model/xreftable.go: `DecodeImageStream` :2284.
- pkg/filter/dctDecode.go: reached only for CMYK DCT.

### 3.6 Properties

- pkg/api/property.go: `Properties` :33 overrides `Cmd` to `LISTPROPERTIES`.
- pkg/pdfcpu/property.go: `PropertiesAdd` :30, `preparePropertiesInfo` :58, then info.go `ensureInfoDict` :110 and write.go `ensureFileID` :253. byblos restores the dates and `/ID` afterwards (properties.go:52-55).

### 3.7 Configuration init (`NewDefaultConfiguration`)

model/configuration.go:942. On the first call it does `os.UserConfigDir()` and then
`ensureDefaultConfigAt` :846:

- `MkdirAll(<dir>/pdfcpu)`.
- `ensureConfigFileAt` :689. This writes or reads `config.yml`, which goes through `readConfiguration` (parseConfig.go:502, YAML).
- `initializeConfigurationResourcesAt` :759. This creates `fonts/` and installs Roboto (`ensureFontDirInitializedAt` :724, `font.InstallFontFromBytesQuiet`), and creates `certs/`.

Later calls return `loadedDefaultConfig.Clone()`. `ConfigPath != "disable"`, so the stateless path
(`NewStatelessConfiguration` :868) is not taken.

**Port decision:** port none of this. pdf-rs supplies a built-in default `Configuration` with the
same values (relaxed validation, `Reader15`, `WriteObjectStream`/`WriteXRefStream` true,
`Optimize` and `OptimizeResourceDicts` true, `OptimizeDuplicateContentStreams` and
`ValidateLinks` false). That removes the filesystem side effects, the YAML parser and the init
race that byblos works around (byb-a5z, pdfdoc.go:70-82).

## 4. Configuration paths byblos selects

### 4.1 Values

- `defaultConfig()` (pdfdoc.go:81-93) clones the defaults. It sets only these `Limits` fields:
  `MaxDecodeBytes`, `MaxObjectCount`, `MaxObjectStreamTotalCount`, `MaxTotalDecodeBytes`,
  `MaxObjectBytes`, `MaxParsedBytes`.
- `openSized` narrows those fields and also sets `MaxStreamTotalBytes` (pdfdoc.go:447-456).
- `RawImage` swaps `MaxDecodeBytes` for each image and restores it afterwards (pdfdoc.go:1417-1420).
- In `MaxDecodeBytes`, 0 means 512 MiB and a negative value means unlimited
  (model/configuration.go:554). byblos never passes a negative value (pdfdoc.go:1352).
- `Cmd` stays at `VALIDATE` except in properties.go:37 (`ADDPROPERTIES`) and inside
  `api.Properties` (`LISTPROPERTIES`).
- Defaults from model/configuration.go:882-896: `ValidationMode=ValidationRelaxed`,
  `ValidateLinks=false`, `WriteObjectStream=true`, `WriteXRefStream=true`, `Optimize=true`,
  `OptimizeResourceDicts=true`, `OptimizeDuplicateContentStreams=false`.
- `UserPW` and `OwnerPW` are always empty.

### 4.2 What these select

- **Read.**
  - Only xref streams and object streams are decoded during read. Ordinary streams are left
    encoded, and only decrypted.
  - Relaxed mode turns on xref repair, the `pageObjType` repair (Count plus Kids means Pages) and
    Float `/Rotate`.
- **Encryption.**
  - Only empty passwords are tried: owner first, then user.
  - If neither works, the result is `ErrWrongPassword`.
  - Owner-password-only files open, and their contents are decrypted on read.
  - The `ErrEncrypted` rejection and the permission checks are not reached.
- **Page tree.** `PageDict` is always called with `consolidateRes=false`. Resource consolidation
  and content parsing are dead on this path.
- **Validate.** Every catalog validator runs in relaxed mode. The link checks are dead.
- **Optimize.**
  - Resource consolidation runs, and parses page content, only when `Cmd=VALIDATE`. That is the
    byblos Optimize path.
  - Content-stream dedup is off.
  - The embedded-font stats code is off.
- **Write fallback.**
  - Xref and object streams are used, unless the source is encrypted and did not use xref streams.
    In that case it falls back to a classic xref table.
  - Strings and streams are re-encrypted with the existing key.

### 4.3 Behaviours pdf-rs must keep, because byblos depends on them

- `PageDict` returns the /Pages node when /Kids is an indirect ref. byblos repairs this in
  `normalizePageTree` (pdfdoc.go:490-540). pdf-rs may fix it in place, but then the byblos port
  must drop its repair.
- `Dereference` treats `*IndirectRef` (the pointer form) as nothing to dereference. byblos builds
  refs in value form for that reason. In Rust there is one `Ref` type, so this quirk disappears.
- `DereferenceStreamDict` returns a copy. Changes to `Content` do not persist (write.go:15-21).
- `Dict.PDFString` silently drops unknown types (dict.go:493). byblos audits for this with
  `auditSerializable`. In Rust, an exhaustive enum makes the drop impossible.
- `DecodeWithLimit` does not enforce the cap on preserved-raw streams (no filter, a sole non-CMYK
  DCT, a sole JPX or JBIG2). byblos re-checks the length itself (limits.go:212).
- Every limit error is an `ErrLimitExceeded`, with the sub-kinds listed in section 2.3. One xref
  limit error is untyped: the string "xref subsection object range exceeds limit", which byblos
  matches as text (pdfdoc.go:463). pdf-rs should give it a typed error variant.

### 4.4 Which byblos features reach the validator

| byblos path | pdf-go call | Reaches validate |
|---|---|---|
| Open, BuildFromPages, Linearize, Write, DanglingRefs | `ReadContext` | no |
| `pdfdoc.Validate` (editpages.go:205) | `api.Validate` | yes |
| Optimize, WriteProperties | `ReadValidateAndOptimize` | yes, plus optimize |
| ReadProperties | `api.Properties` | yes, plus optimize |
| FontDicts (`cmd/byblos-fonts` only) | `ReadContextFile` | yes |

## 5. Third-party Go dependencies and the Rust plan

There are no C dependencies.

| Go dependency | Where it is reached | Rust plan |
|---|---|---|
| `compress/zlib`, `compress/flate` (stdlib) | Flate decode and encode, `StreamDict.Encode`, xref/ObjStm write, PNG output | `dobbo-ca/deflate-rs`. The Adler-32 trailer and zlib header are a few lines in-crate. |
| `crypto/md5`, `crypto/rc4`, `crypto/aes` + CBC, `crypto/sha256`, `crypto/sha512`, `crypto/subtle` | crypto.go read side (R2-R6 key derivation, RC4-40/128, AESV2/V3, R6 `hashRev6`), and the write fallback | Port in-crate: MD5, RC4, AES-128/256 CBC decrypt and encrypt, SHA-256/384/512. Constant-time comparison. |
| `crypto/rand` | `encryptAESBytes` IV (write fallback only) | OS RNG via `getrandom`, or drop it with the encrypted write fallback (see the open questions). |
| `math/big` | `hashRev6` (the mod-3 step) | Not needed: the mod 3 of a 16-byte big-endian number is a byte-sum mod 3. |
| `github.com/xdg-go/stringprep`, `golang.org/x/text/unicode/norm` | crypto_password.go (SASLprep on R5/R6 passwords) | Not ported. byblos only uses empty passwords, and SASLprep of "" is "". Hard-code the empty-password path and reject a non-empty password with "unsupported". |
| `go.yaml.in/yaml/v3` | parseConfig.go, on the first `NewDefaultConfiguration` | Not ported. Use built-in defaults (section 3.7). |
| `golang.org/x/image/ccitt` | filter/ccittDecode.go (CCITT image or stream decode) | Port the CCITT G3 1-D and G4 decoder in-crate (about 800 lines in the Go x/image package). K>0 stays unsupported, as it is in Go. |
| `image/jpeg` (stdlib) | filter/dctDecode.go, for CMYK DCT only (`renderDCTToPNG`) | `gojpeg.rs` (in-crate port of Go's `image/jpeg`); jpeg-rs supplies only the result image types and the test encoder. All other DCT stays as encoded passthrough. |
| `image/png` (stdlib) | writeImage.go PNG output | A minimal PNG encoder in-crate: IHDR/IDAT/IEND, CRC-32 and deflate-rs. |
| `github.com/hhrutter/tiff` | writeImage.go:428, :780 (non-DCT CMYK output as TIFF) | A minimal uncompressed baseline TIFF writer in-crate (8-bit CMYK, single strip). |
| `encoding/gob` | an internal handoff between the DCT decode and the PNG render | Not ported. Pass the pixel buffer directly. |
| `encoding/ascii85`, `encoding/hex`, `encoding/xml`, `bufio`, `time`, `unicode/utf16` | filters, `HexLiteral`, validate/metaData.go (XMP), the reader, dates, text strings | Port the small encodings in-crate. For XMP, see the validate open question; a minimal well-formedness check is enough for relaxed mode, but that is not verified. |
| `golang.org/x/image/webp` | blank import at model/image.go:39 (link-time only) | Not ported. |
| `golang.org/x/text/encoding/charmap` | font/fontDict.go:991 (not on the `Name` path) | Not ported. |
| `golang.org/x/crypto` | pkg/pdfcpu/sign/revocate.go only | Not reached. Not ported. |
| `spf13/cobra`, `google/jsonschema-go`, `mattn/go-runewidth`, `clipperhouse/uax29` | cli and config schema | Not reached. Not ported. |

## 6. Out of scope

pdf-rs does not port any of the following.

### Whole packages

- `pkg/cli` and `cmd/`
- `pkg/pdfcpu/sign` and `pkcs7`; certificates (certificate*.go)
- `pkg/pdfcpu/form`, `primitives`, `create`, `draw`, `color`, `format`, `matrix`
- `pkg/pdfcpu/font`, except `Name` and `ErrCorruptFontDict`
- `internal/corefont`, `internal/netutil`, `internal/inputlimit`, `internal/fileutil`
- `pkg/samples`, `pkg/testdata`

### pdfcpu features byblos never calls

stamp and watermark, nup, booklet, cut, zoom, resize, merge, page insert and remove, bookmark,
annotation editing, attachments, keywords, `importImage`, extracting pages, fonts, content or
metadata, `ExtractPageImages`, `ExtractImagesRaw`, `api.Optimize`/`OptimizeFile`, `WriteIncrement`,
`writeFlatObject`, stats files.

### Encryption

Not ported: creating or changing encryption (`ENCRYPT`, `DECRYPT`, `SETPERMISSIONS`,
`newEncryptDict`, `calcOAndU*`, `calcFileEncKey`, `writePermissions`), the public-key handler
(`Adobe.PubSec`), non-empty passwords, and the permission checks.

### Configuration

Not ported: config files, YAML, the schema, `LoadConfiguration` and `ResetConfiguration`, the font
and certificate directories, `NewAESConfiguration` and `NewRC4Configuration`.

### Code left out of files that are otherwise in scope

- optimize.go: the content-stream dedup (:46-247) and the stats-only font and image sizing
  (:1365-1590).
- writeImage.go: `WriteImage`.
- extract.go: `imageStub`, which is what `stub=true` would need.
- model/image.go: everything except the `Image` struct and the image limits.
- model/xreftable.go: the page-boundary, insert and blank-page code, and the name-tree editing.
- types: `layout.go`, `paperSize.go`, `anchor.go`, `Reverse`, `QuadLiteral`, `Dim`, the
  `ObjectStreamDict` builders.
- filter: every `Encode` except Flate, the LZW writer.
- validate: the link checking and the strict-only branches.

## 7. Corrections to the input traces (verified against the source)

- `ExtractImage`'s signature is `(c, ctx, sd, thumb, resourceID, objNr, stub)`
  (extract.go:511). byblos passes `thumb=false` and `stub=false`. One input had the two labels
  swapped; behaviour is the same because both are false.
- `api.Validate` and `ReadValidateAndOptimize` take `*ProgressOptions` as their 4th argument, not
  a configuration (validate.go:118, api.go:304).
- `pkg/filter` is 1,561 non-test lines, not 3,153. `internal/filter/lzw` is 510 lines (reader plus
  writer).
- YAML *is* reached. The first `NewDefaultConfiguration` reads or writes `config.yml` through
  `readConfiguration` (configuration.go:689-723, parseConfig.go:502). One trace said
  parseConfig.go was not reached.
- `golang.org/x/crypto` is imported only by `pkg/pdfcpu/sign/revocate.go`. It is not on any byblos
  path. One trace listed it for crypto.go, which imports no third-party package.
- `types/utf16.go` imports only the stdlib. `EscapedUTF16String` does not reach `x/text`.
- `skipTJ` (model/parseContent.go:115) returns an error and does not panic at 113d693. The byblos
  comments at pdfdoc.go:131-136 describe older behaviour. It does not matter, because
  `consolidateRes=false` is used everywhere.
- `FontDicts`, the only caller of `ReadContextFile`, is used only by `cmd/byblos-fonts/main.go:159`.

## 8. Decisions (were open questions)

The brief fixes the rule: port everything that byblos reaches, and keep Go behaviour so the
parity oracle can diff it. So:

1. **Validator.** Ported in full for relaxed mode (the mode byblos sets), with its side effects
   (`Valid` flags read by the writer). Strict-only checks are out of scope.
2. **Encrypted write fallback.** Ported: byblos reaches it for encrypted and rootless tables.
3. **Optimize.** Ported in full, including resource consolidation as Go does it.
4. **Page-tree quirks.** Kept as Go has them; parity depends on them.
5. **CCITT.** K>0 stays unsupported, with the same error class as Go.
6. **Not traced.** The validate files, `writeStats.go` and the write-side `PDFString`
   encryption are traced file by file during the port; whatever they reach on the byblos paths
   is in scope.
