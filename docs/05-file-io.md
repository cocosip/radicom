# Module Spec: File and IO (Buffers, Parser, Writer, DicomFile)

> Status: design phase (milestone M3; see [03-development-plan.md](03-development-plan.md) §3)
> Feature IDs covered: BUF-01..02, IO-01..03, RD-01..05, WR-01..03, FILE-01..06
> Behavior baseline: fo-dicom `IO/` (Reader, Writer, Buffer), `DicomFile.cs`, `DicomFileReader.Preprocess`, tolerance behaviors accumulated in fo-dicom issue history

---

## 1. Buffer Layer (BUF-01/02)

### 1.1 Trait

```rust
pub trait ByteBuffer: Send + Sync {
    fn size(&self) -> u64;
    fn is_memory(&self) -> bool;                                  // true → data() is cheap
    fn data(&self) -> Cow<[u8]>;                                  // materialize (may lazy-load)
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]);        // bounds-checked
    fn copy_to(&self, w: &mut impl Write) -> io::Result<u64>;     // chunked 1 MiB
}
```

### 1.2 Implementations

| Type | Semantics |
|---|---|
| `Memory(Bytes)` | Owning bytes; `is_memory = true` |
| `Empty` | Singleton; size 0 |
| `Range { inner: Arc<dyn ByteBuffer>, offset, len }` | Zero-copy view; passes `is_memory` through |
| `Composite(Vec<Arc<dyn ByteBuffer>>)` | Concatenation view; cross-segment range reads |
| `File { path, position, len }` | Lazy window into a file (read-large-on-demand); opens per access, chunked |
| `Stream { stream: Arc<Mutex<...>>, position, len }` | Window into a seekable stream; **mutex serializes seek+read** |
| `Endian { inner, unit }` | Byte-swap view per `unit` (2/4/8); no-op when unit==1 |
| `EvenLength { inner }` | Appends one pad byte when the inner size is odd (write-side) |
| `BulkDataUri { uri, cell: OnceCell<Bytes> }` | JSON bulkData reference; `data()` errors until filled |
| `Lazy { cell: OnceCell<Bytes>, f }` | Deferred materialization (string elements) |

Values ≥ the large-object threshold coming from a file source must arrive as `File`/`Stream` windows under `ReadLargeOnDemand`, or as `Composite(Memory..)` chunks when a single value exceeds the max array length.

## 2. Byte Source / Target (IO-02/03)

- `ByteSource`: `position/endian/is_eof/require(n)/skip/goto/get_u8..get_u64/get_f32/get_f64/get_bytes/get_buffer(len) -> ByteBuffer` (large-object policy hook), + `read_back(n)` used by format probing. Implementations: `ByteSourceReader<R: Read>` (with optional seek fast path) and `ByteSourceFile` (std File; positions preserved for on-demand windows).
- `ByteTarget`: `write_u16../write_bytes/write_buffer(&dyn ByteBuffer)` streaming through 1 MiB chunks; `as_inner_writer()`.
- `Endian` helpers and `ByteConverter` (buffer ↔ typed slices with endianness).
- Minor utilities verified during the coverage audit: a sequential multi-stream concatenation reader (fo-dicom `MergeStream`), typed element-wise enumeration over buffers (`ByteBufferEnumerator`), and a buffer-to-source adapter (`ByteBufferByteSource`) — plain internal helpers, no separate features.

## 3. Reader (RD-01..05)

### 3.1 Core loop

Recursive-descent, one function per stage:

```
parse_dataset():
  while !eof:
    tag    = parse_tag()      // group, element; interned; private creator resolution (3.5)
    vr     = parse_vr(tag)    // see 3.2
    length = parse_length(vr)
    parse_value(tag, vr, length)
```

Result enum: `Success | Stopped | Suspended | Error`. `require(n)` failing at any stage → `Suspended` (resumable for streaming); user stop predicate hit → rewind to element start and return `Stopped`. SQ depth guard: `max_depth = 256` → `Error::Reader(TooDeep)`.

### 3.2 VR resolution rules (RD-01/03)

1. Item (`FFFE,E000`), item delimiter (`FFFE,E00D`), sequence delimiter (`FFFE,E0DD`) → internal item VRs regardless of syntax.
2. Explicit syntax: read 2 bytes.
   - Parse failure and bytes are `0x20 0x20` → use dictionary first VR (fo-dicom #179 workaround), continue explicit.
   - Other parse failure → **fallback**: rewind 2 bytes, continue as implicit for this element.
3. Implicit syntax: VR from dictionary; unknown tag → `UN`; dictionary entry OB-or-OW → `OW`; `(gggg,0000)` group length → `UL`.
4. Private creator slots (odd group, element 0x0010..0x00FF) → `LO`.
5. Explicit context encountering `UN` on an element whose dictionary VR is SQ-compatible: attempt SQ reinterpretation (see 3.4).

### 3.3 Length rules

Explicit short VRs: 2-byte; explicit long VRs (and OB/OD/OF/OL/OV/OW/SQ/UC/UR/UT/SV/UV/UN): 2 reserved + 4-byte; implicit: 4-byte; items: 4-byte. `0xFFFFFFFF` = undefined length (SQ or fragment).

### 3.4 Value parsing and SQ handling (RD-01/03)

- Defined-length SQ: emit `on_begin_sequence(len)`, parse exactly `len` bytes as items, emit item events; no delimiter expected.
- Undefined-length SQ: parse items until `FFFE,E0DD`.
- Undefined-length non-SQ → fragment sequence: first fragment is the **offset table** (4-byte units), subsequent fragments per `vr.unit_size`; `FFFE,E0DD` terminates. Unexpected tag inside a fragment sequence → `Error::Reader`.
- **UN-encoded SQ recovery**: value with VR `UN` + undefined length → parse as SQ; additionally, under big-endian syntaxes an UN-encoded SQ is parsed little-endian inside (CP-246).
- **Private SQ explicit/implicit mismatch probe**: for a private tag with undefined length whose dictionary VR is unknown: peek ahead — if the next 4 bytes are an item tag and the following 8 bytes look like a valid explicit VR header, the writer encoded it explicitly while the dataset is implicit (or vice versa); switch parsing mode for that subtree (fo-dicom #223/#487 behavior).
- **Orphan item**: an item tag at dataset level without a preceding SQ → treat as an implicit undefined-length SQ header (fo-dicom #364).
- Deflate: `TransferSyntax.is_deflate` → wrap the whole source in a `FlateRead` before parsing.

### 3.5 Private creator tracking (RD-05)

The parser maintains `HashMap<(group, block), creator>` while descending: any parsed LO value in a creator slot updates the map; subsequent private tags in the same group resolve their creator from it. The map is per-parse (not global).

### 3.6 Stop predicates (FILE-01)

`StopFn = FnMut(&ParseState) -> bool` with `ParseState { previous_tag, tag, sequence_depth }`. On stop: source rewinds to the element start; result `Stopped`; `DicomFile.is_partial = true`.

### 3.7 Observer (RD-02)

`ParseObserver` as drafted in architecture §4.3. The dataset builder observer additionally: maintains a dataset stack; switches the encoding stack when `(0008,0005)` values arrive; constructs typed elements per VR; rejects fragment sequences with non-OB/OW VR; stores `None` buffers as skip markers under `SkipLargeTags`. A per-tag callback observer (fo-dicom `DicomReaderCallbackObserver` + multi-observer broadcast) is provided as a composable adapter — the file reader uses it internally to intercept `TransferSyntaxUID` and the ACR-NEMA recognition code.

### 3.8 Tolerance corpus mapping (RD-03) — every row is a mandatory test

| # | Scenario | Behavior |
|---|---|---|
| T1 | Explicit VR bytes are two spaces | dictionary VR, stay explicit |
| T2 | Explicit VR garbage | rewind, parse element implicit |
| T3 | UN + undefined length on SQ | parse as SQ (LE inside if BE, CP-246) |
| T4 | Private SQ encoded explicit inside implicit dataset (and inverse) | probe + mode switch for subtree |
| T5 | Orphan item tag | implicit undefined SQ |
| T6 | Fragment sequence missing final delimiter at EOF | accept (fo-dicom #1339) |
| T7 | SQ ends directly with sequence delimiter while item expected (k-pacs style) | accept |
| T8 | Trailing garbage after dataset | ignore |
| T9 | Group length element with UN VR | treat as UL |
| T10 | Deflated syntax | transparent decompress |

## 4. File-Level Reading (FILE-01..03)

### 4.1 Format detection algorithm (FILE-03)

```
preprocess(source):
  if size >= 132 and bytes[128..132] == b"DICM": format = Dicom3; skip 132
  else:
    rewind; read first tag's group (2 bytes, LE guess)
    if group > 0x00FF: big endian; assume explicit BE
    if group == 0x0002: format = Dicom3NoPreamble
    else: format = Dicom3NoFileMetaInfo
    determine explicit/implicit: compare the 2 VR bytes at offset+4 against the dictionary VR
      of that tag (valid two-letter code match ⇒ explicit); loop over successive tags until decided
    implicit + big-endian ⇒ virtual ImplicitVRBigEndian syntax
  parse file meta (always Explicit VR LE) until group > 0x0002
  read RecognitionCodeRETIRED (0008,0010) if present: "ACR-NEMA 1.0"/"2.0" ⇒ format ACRNEMA1/2
  TransferSyntaxUID from meta decides the dataset syntax (incl. deflate)
```

Minimum input: 132 bytes or an error `Reader::InvalidFile`.

### 4.2 Read options (FILE-02)

`ReadOptions { read: FileReadOption, large_object_size: u64 (default 64 KiB), fallback_encoding }`:

| Option | Value ≥ threshold |
|---|---|
| `ReadLargeOnDemand` | `File`/`Stream` window buffer; source stays open (documented lifetime hazard) |
| `SkipLargeTags` | element skipped (no buffer recorded; fragment fragments dropped) |
| `ReadAll` | `Memory` buffer (values above max-array-length become `Composite`) |
| `Default` | alias of `ReadLargeOnDemand` |

Non-seekable streams are wrapped in a buffered reader with pushback; `ReadLargeOnDemand` degrades to `ReadAll` for them.

### 4.3 DicomFile assembly (FILE-01/04)

- Meta table built by an observer restricted to group 0002 (enforced: other groups error).
- After parse: set `dataset.transfer_syntax` from meta; `is_partial` per 3.6; wrap any failure as `Error::File { stage, source }` carrying the path/stream identity.

## 5. Writer (WR-01..03)

### 5.1 Element encoding rules

| Syntax | Layout |
|---|---|
| Explicit LE/BE | tag(4) + vr(2) + len(2 or 2 reserved+4) + value(padded to even with `vr.padding`) |
| Implicit | tag(4) + len(4) + value(padded) |

- Values are obtained via `element.to_bytes(encodings)`; buffers whose endianness already matches the target are written directly (`Endian` wrappers unwrapped), otherwise swapped per `unit_size`.
- Long-VR length > 0xFFFE under a 16-bit-length VR (only possible for delimiters — practically never) → CP-1066 demotion to `UN` with 32-bit length; implemented as a guard.

### 5.2 Sequences and fragments

- Default: undefined-length SQ + per-item undefined length + delimiters.
- `WriteOptions { explicit_length_sequences: false, explicit_length_sequence_items: false, keep_group_lengths: false, large_object_size: 1 MiB }`.
- **Private SQs are always written with explicit lengths** (fo-dicom rule; avoids reader ambiguity).
- Explicit lengths require the length calculator (5.3) — exact, recursive, syntax-aware (header size + value + delimiters when undefined).
- Fragments: undefined-length header, offset-table item (`len = 4 × offsets`, each offset points to the first byte of the frame's first fragment relative to the first fragment's item start), one item per fragment, `FFFE,E0DD` terminator.

### 5.3 File writing (FILE-05)

```
save(file):
  preprocess meta:
    ACRNEMA formats → Error::File(FormatNotSaveable)
    Dicom3NoFileMetaInfo → rebuild meta from dataset (SOPClass/InstanceUID required)
    otherwise refresh: ImplementationClassUID/VersionName = radicom constants
  write 128-byte preamble + b"DICM"
  write meta with forced Explicit VR LE after recalculating group lengths
  dataset: on_before_serializing hook (charset finalization)
    keep_group_lengths ? recalculate : remove (PS3.5 7.2)
    deflated syntax → render to memory, deflate, write
```

`has_valid_header(path)`: seek 128, check `DICM` (FILE-06).

## 6. Files, Temp Files, Directories (IO-01)

- `FileRef` trait (open/create/delete/move/range read) + default `PathBuf` impl; factory is a plain function pointer/type-state (no DI).
- `TempFileProvider`: configurable directory (default system temp); names UUID-based; files carry `temporary` attribute hint; a background deleter retries locked files (Windows semantics) and drops the remainder at exit.
- `DirectoryRef`: enumerate files/subdirs with glob filter (used by the media scanner, MEDIA-03).

## 7. Test Requirements Checklist

- [ ] Buffer matrix: every implementation against the trait contract (sizes, ranges, chunked copy).
- [ ] Parser: synthetic streams per tolerance row T1–T10; stop predicate rewind correctness; depth guard; suspended/resumed parse over chunked input.
- [ ] Format detection golden cases: DICOM3 / no-preamble / no-meta / ACR-NEMA / BE implicit (virtual syntax) / GE swap.
- [ ] Round-trip: read→write→read byte-identical for: explicit LE/BE, implicit, undefined vs explicit SQ, fragments, deflate, multi-charset.
- [ ] Read options: on-demand windows produce identical values after re-read; skip leaves placeholders; large value > max array length composites.
- [ ] Writer: group length recalc/remove; private SQ explicit; CP-1066 guard unit; meta refresh behavior.
- [ ] Temp file lifecycle (create, defer-delete, exit cleanup) on Windows and Unix.
