# Development Guide

> Planning, feature IDs, progress and testing. Design invariants live in
> [01-architecture.md](01-architecture.md). Historical day-by-day entries are **not** kept here — use
> `git log`.

## 1. Scope

**In scope (`radicom`)**: DICOM data model, file reading/writing, network communication (DIMSE),
serialization, media files, structured reports, printing data model, anonymization, pixel data access
facade, codec **interfaces** and transcoder, rendering pipeline.

**Out of scope (`radicom-codecs`, a separate project)**: the concrete **implementations** of JPEG /
JPEG-LS / JPEG 2000 / HTJ2K / RLE codecs. `radicom` defines only the interfaces, parameter system,
registration mechanism and transcode flow. `radicom-codecs` mirrors `fo-dicom.Codecs` and injects its
implementations through the registration point.

**Priorities**

| Level | Meaning |
|---|---|
| P0 | Core requirement, part of the MVP; the library is unusable without it |
| P1 | Important; should be done before the official release |
| P2 | Enhancement; may be deferred |

**fo-dicom components with no direct port** (the capability is covered in a Rust-native shape; only the
.NET-specific packaging is not carried over):

| fo-dicom component | radicom equivalent |
|---|---|
| `FO-DICOM.AspNetCore` (IHostedService wrapper) | HOST-01: config-driven hosting helper on `radicom-network` |
| `FO-DICOM.Instrumentation` | OBS-02: `tracing`-convention metrics emission |
| `FO-DICOM.Json` (Newtonsoft backend) | JSON-01 (serde). A second backend is redundant — divergence D11 |
| `FO-DICOM.Dump` (WPF GUI) | INF-07: `radicom-dump` CLI + INF-04 library dump |
| WinForms / WPF / SkiaSharp / NetStandard image backends | REND-07: one `RawImage` + optional backend — divergence D7 |
| GDI+ print rendering extensions | PRINT-02: cross-platform FilmBox layout rendering |
| Managed codecs in Core (RLE, JPEG Lossless) | radicom ships zero built-in codecs — divergence D10 |
| Legacy client plumbing (`DicomClientConnection`, state shims) | replaced by the async expert layer (CLI-02) |

---

## 2. Feature List

Every feature carries a stable ID. **IDs are never renumbered**; a superseded feature is marked
`Dropped` with a reason rather than deleted. These IDs are referenced throughout the code and the
architecture document.

### 2.1 Core identifier system

| ID | Feature | Priority |
|---|---|---|
| TAG-01 | Tag structure `(group, element)`; `is_private`; u32 conversion; Display/Parse formats | P0 |
| TAG-02 | Private tag semantics: equality/ordering/hash by group + low 8 bits + creator; block normalisation | P0 |
| TAG-03 | 5259 generated `Tag` constants plus 88 masked rows from the fo-dicom dictionary XML | P0 |
| TAG-04 | Tag interning index (u32 → Tag) so the parser and AT elements reuse instances | P1 |
| TAG-05 | `MaskedTag` (card + mask, `xx` wildcard) with Parse/Display/IsMatch | P1 |
| VR-01 | The 34 VRs with encoding metadata (length-field width, padding, unit size, string flags) | P0 |
| VR-02 | VR parsing from both strings and two-letter bytes | P0 |
| UID-01 | UID typing (12 classifications) plus heuristic storage-category derivation | P0 |
| UID-02 | UID registry (~1928 generated constants) with Parse/Register/Query/Enumerate | P0 |
| UID-03 | UID generation: `2.25.<uuid>`, sequence append, consistent remapping, recursive regeneration | P0 |
| UID-04 | Print management Meta SOP ↔ sub SOP class mapping, both directions | P2 |
| UID-05 | Derive a DICOM UID from the host MAC address | P2 |
| TS-01 | ~57 built-in transfer syntaxes with explicit/encapsulated/lossy/deflate/endianness attributes | P0 |
| TS-02 | Transfer syntax Parse/Lookup/Query/Register — the extension point for `radicom-codecs` | P0 |
| TS-03 | Virtual syntaxes (Implicit VR Big Endian, GE private big endian) | P1 |
| DICT-01 | Dictionary entries (tag/masked/name/keyword/VR list/VM/retired); public + private, lazy loaded | P0 |
| DICT-02 | Lookup precedence: private sub-dictionary → creator fallback → exact → masked → Unknown | P0 |
| DICT-03 | Runtime loading of external dictionaries from XML; one-shot default replacement | P1 |
| VM-01 | VM representation with Parse/Display round-trip | P1 |

### 2.2 Dataset and elements

| ID | Feature | Priority |
|---|---|---|
| DS-01 | Ordered dataset container (tag-sorted) with deep-copy and transfer-syntax cascade into nested items | P0 |
| DS-02 | Three read tiers: `get_*` (Err), `try_get_*` (Option), `get_or` (default) | P0 |
| DS-03 | Write API: insert/insert_or_update, VR resolution, multi-value parsing, remove/clear | P0 |
| DS-04 | Private tag operations with automatic creator insertion and block-exhaustion handling | P0 |
| DS-05 | `clone` (deep, skip validation) and `copy_to` (all / selected / masked filter) | P0 |
| DS-06 | Enhanced multi-frame functional-group flattening | P1 |
| DS-07 | Element-wise deep content equality as `PartialEq` | P1 |
| DS-08 | DA + TM → timezone-aware date-time composition | P1 |
| DS-09 | Walker trait (element / SQ enter-exit / fragment enter-exit), shared by serializers and dumps | P0 |
| DS-10 | `enumerate_group` (group length excluded) and `enumerate_masked` | P2 |
| ELEM-01 | Concrete element types per VR (strings, PN, numeric strings, date-time, binary, AT, Other, UN, UI) | P0 |
| ELEM-02 | Conversion matrix to Rust types; DS comma tolerance; precision demotion | P0 |
| ELEM-03 | Fragment sequences: offset table + fragment list, OB/OW variants | P0 |
| ELEM-04 | Sequence elements with recursive validation | P0 |
| ELEM-05 | Lazy evaluation: strings keep text only, encoding and padding deferred to serialization | P1 |
| MATCH-01 | Match rule DSL (14 rules plus And/Or/Negate composition) | P2 |
| XFORM-01 | Transform rule DSL (16 rules) with Modified Attributes Sequence recording | P2 |

### 2.3 Character sets and validation

| ID | Feature | Priority |
|---|---|---|
| ENC-01 | ~34 charset ↔ encoding mappings with spelling tolerance and ASCII fallback | P0 |
| ENC-02 | Multi-charset decoding via ISO 2022 escape sequences; per-VR reset rules | P0 |
| ENC-03 | Multi-charset encoding: strict probing, per-segment trial, escape injection, fallback | P1 |
| ENC-04 | Custom charset registration and encoding → charset reverse lookup | P1 |
| VAL-01 | Per-VR string validators (length, legal characters, format, ranges) incl. DT timezone rules | P0 |
| VAL-02 | VM validation against dictionary Min/Max | P1 |
| VAL-03 | Validation switches: global × per-dataset × scope guard | P0 |

### 2.4 File and IO

| ID | Feature | Priority |
|---|---|---|
| FILE-01 | `DicomFile` open from path/stream/bytes, stop predicate, context-carrying errors | P0 |
| FILE-02 | Large-object policy: Default / ReadLargeOnDemand / SkipLargeTags / ReadAll with threshold | P1 |
| FILE-03 | Format detection: preamble/no-preamble/no-meta/ACR-NEMA, endianness, VR explicitness | P0 |
| FILE-04 | File meta information (group 0002) with strongly typed fields and pre-save refresh | P0 |
| FILE-05 | Save: preamble + meta (always Explicit VR LE, group length recalculated) + dataset | P0 |
| FILE-06 | `has_valid_header` and Clone | P1 |
| RD-01 | Recursive-descent parser with depth cap (256) | P0 |
| RD-02 | 8-callback parse observer trait, decoupling parsing from building | P0 |
| RD-03 | Malformed-data tolerance (rows T1–T10, see [01-architecture.md](01-architecture.md) §7) | P0 |
| RD-04 | Deflate transfer syntax (raw DEFLATE on write, zlib fallback on read) | P1 |
| RD-05 | Private creator tracking during parsing; tags bind their creator immediately | P0 |
| WR-01 | Walker-based writer; CP-1066 UN demotion; private SQ always explicit length; fragments | P0 |
| WR-02 | Exact encoded-size calculator (reused by explicit lengths and DICOMDIR offsets) | P0 |
| WR-03 | Group length recalculation/removal plus write options | P0 |
| BUF-01 | `ByteBuffer` trait (`is_memory/size/data/get_byte_range/copy_to`) | P0 |
| BUF-02 | Buffer implementations: Memory/Empty/Range/Composite/FileWindow/StreamWindow/TempSpill/Endian/padding/BulkDataUri | P0 |
| IO-01 | File/directory references and temporary files with deferred deletion | P1 |
| IO-02 | Byte source/target with multi-width access and large-object logic | P0 |
| IO-03 | Endianness enum, byte swapping, `ByteConverter` | P0 |
| MEM-01 | Large-buffer lease pool | P2 |

### 2.5 Pixel data and codec interfaces

| ID | Feature | Priority |
|---|---|---|
| PIX-01 | `PixelData` facade: attribute mapping, OB/OW/encapsulated kinds, factories | P0 |
| PIX-02 | Frame access: zero-copy slices, three encapsulated extraction rules, GE big-endian swap, `add_frame` | P0 |
| PIX-03 | Frame size with 1-bit row packing, YBR 422/420 odd widths and subsampling | P0 |
| PIX-04 | Palette LUT extraction (size 0 → 65536; 16-bit entries take the high byte) | P1 |
| PIX-05 | Overlay discovery, standalone and embedded high-bit extraction, reverse building | P1 |
| CODEC-01 | `Codec` trait touching only the `PixelData` facade — the narrow boundary for `radicom-codecs` | P0 |
| CODEC-02 | Codec parameter families (JPEG, JPEG 2000, HTJ2K, RLE, JPEG-LS) | P0 |
| CODEC-03 | `CodecRegistry`; default empty; `radicom-codecs` injects its own implementation | P0 |
| ~~CODEC-04~~ | ~~Built-in RLE codec~~ — **dropped 2026-10-27** (zero built-in codecs; divergence D10) | – |
| CODEC-05 | JPEG SOF bit-depth probing (a shared boundary utility, not a codec) | P2 |
| TR-01 | `Transcoder` with the decision table, lossy bookkeeping, overlay extraction, meta sync | P0 |
| TR-02 | Single-frame cloning decode (`decode_frame`, `decode_pixel_data`) | P0 |
| TR-03 | `clone_with_syntax` convenience API | P1 |

### 2.6 Network (DIMSE)

| ID | Feature | Priority |
|---|---|---|
| NET-01 | Async TCP/TLS abstraction: options, timeouts, initiator/acceptor, IPv4/IPv6 | P0 |
| NET-02 | PDU codec for all 7 types with bounds checking and PDV handling | P0 |
| NET-03 | Association model: presentation contexts, odd IDs, role selection | P0 |
| NET-04 | Extended negotiation (SOP class and common) plus user identity | P1 |
| NET-05 | Protocol engine: receive loop, backpressure, pending table, PDV reassembly | P0 |
| NET-06 | Streaming send: on-the-fly PDV chunking, circuit breaker, on-the-wire transcoding | P0 |
| NET-07 | Timeouts and watchdog with pending cleanup | P1 |
| NET-08 | Connection lifecycle, graceful/forced close, typed error propagation | P0 |
| SRV-01 | Generic `DicomServer<P>`: listening, per-connection tasks, **enforced max-clients**, lifecycle | P0 |
| SRV-02 | Built-in C-ECHO SCP | P0 |
| SRV-03 | C-STORE receive with an injectable spool sink; failures answered without dropping the connection | P0 |
| SRV-04 | C-FIND with level presets and streamed matches | P0 |
| SRV-05 | C-GET with reverse C-STORE sub-operations | P1 |
| SRV-06 | C-MOVE with outbound store and Move Originator written | P1 |
| SRV-07 | N-EVENT-REPORT / N-GET / N-SET / N-ACTION / N-CREATE / N-DELETE | P2 |
| SRV-08 | C-CANCEL (fo-dicom defines it without implementing; radicom fills the gap) | P2 |
| CLI-01 | High-level `DicomClient`: queue, association reuse, retry, cancellation, events | P0 |
| CLI-02 | Expert layer: `Connection`, response streams, manual association management | P1 |
| HOST-01 | Config-driven server hosting helper with health and graceful stop | P2 |
| DIMSE-01 | Command model: 22 fields, bit-15 response flag, message IDs, ~60 status presets with masks | P0 |
| OBS-01 | Structured logging plus a metrics collection trait | P1 |
| OBS-02 | `tracing`-convention metric emission (no `opentelemetry` dependency — divergence D12) | P2 |

### 2.7 Serialization, media, SR, printing, anonymization

| ID | Feature | Priority |
|---|---|---|
| JSON-01 | DICOM JSON (PS3.18): tag keys, Value/InlineBinary/BulkDataURI, PN groups, numeric modes, **group length stripped**, tolerant reading | P1 |
| JSON-02 | Bidirectional BulkDataUri buffer references | P2 |
| XML-01 | One-way Native DICOM Model export | P2 |
| MEDIA-01 | DICOMDIR read/write: two-pass offsets, four-level dedup chain, nearest-offset tolerance | P1 |
| MEDIA-02 | `IconGenerator` trait plus the built-in 8-bit ≤128×128 builder | P2 |
| MEDIA-03 | Directory scanner with progress events | P2 |
| SR-01 | SR content tree: 15 value types, 7 relationships, traversal, open/save with explicit lengths | P1 |
| SR-02 | CodeItem (short/long/URN dispatch), MeasuredValue, ReferencedSOP | P1 |
| PRINT-01 | Film Session/Box, Image Box, Presentation LUT models with layout computation | P2 |
| PRINT-02 | FilmBox layout rendering to an image | P2 |
| ANON-01 | PS3.15 Basic Profile: 11 options × 6 actions, wildcard matching, SQ recursion, consistent UIDs | P1 |

### 2.8 Rendering

| ID | Feature | Priority |
|---|---|---|
| REND-01 | `DicomImage` entry: PI inference, window properties, frame caches, scale, overlay compositing | P2 |
| REND-02 | Pipelines: grayscale (Modality → VOI → output table → invert), RGB, palette | P2 |
| REND-03 | LUT family: VOI, Modality, Output, Palette, Invert, Padding, Composite | P2 |
| REND-04 | `GrayscaleRenderOptions` factory chain plus the XA/XRF exception | P2 |
| REND-05 | Pixel render types: bit masking, min/max, histogram, bilinear/nearest scaling | P2 |
| REND-06 | Color conversion: YBR family → RGB, planar↔interleaved, reverse bits, color tables | P2 |
| REND-07 | Image backend: dependency-free `RawImage` (BGRA) with PNG/BMP export | P2 |
| REND-08 | `FrameGeometry` (image ↔ patient) and localizer projection/intersection | P2 |
| REND-09 | MPR: volume cutting, stack resampling, write-back | P2 |
| REND-10 | Icon image rendering `(0000,9113)` | P2 |
| REND-11 | SpatialTransform plus shared math utilities (Point/Vector/Matrix/Rect/Interval/Histogram/BitList) | P2 |

### 2.9 Infrastructure

| ID | Feature | Priority |
|---|---|---|
| INF-01 | Error hierarchy carrying tag/VR/content context | P0 |
| INF-02 | ImplementationClassUID / VersionName constants | P0 |
| INF-03 | Logging facade with categorized targets and dataset dump | P1 |
| INF-04 | Dataset → formatted string dump | P1 |
| INF-05 | Code generation (tags, UIDs, dictionaries, anonymization CSV) | P0 |
| INF-06 | Test data corpus and round-trip baselines | P0 |
| INF-07 | `radicom-dump` CLI | P2 |

**Totals**: 127 items (126 live + 1 dropped), 65 at P0, 32 at P1, 29 at P2.

---

## 3. Milestones and Progress

Milestones are ordered so each delivers a usable, testable increment.

| Milestone | Theme | Status |
|---|---|---|
| M0 | Project scaffolding (workspace, 4 crates, code generation, CI, test data) | Done |
| M1 | Identifier system (Tag/VR/UID/TS/Dictionary/VM) | Done |
| M2 | Dataset, elements, charsets, validation | Done |
| M3 | File and IO (buffers, parser, writer, DicomFile) | Done |
| M4 | Pixel data, codec interfaces, transcoder — **codec boundary frozen** | Done |
| M5 | Network core (PDU, association, engine, C-ECHO, C-STORE, TLS) | Done |
| M6 | Network services (C-FIND/GET/MOVE, expert layer, extended negotiation) | Done except real-peer MOVE |
| M7 | Serialization, media, SR, anonymization | Done |
| M8 | Rendering, printing, rule DSLs, hosting, observability, benchmarks | Done (image-crate backend optional) |

Outstanding work is environment- or scope-limited, not missing implementation:

| Item | Category | Reason |
|---|---|---|
| JPEG / JPEG-LS / JPEG 2000 / HTJ2K / RLE implementations | Out of scope | Belongs to the separate `radicom-codecs` project |
| UID-05 macOS MAC | Platform | Needs CoreWLAN/IOKit FFI; Windows and Linux are implemented and tested |
| Reference-peer interop regression | Environment | Needs an external DCMTK/fo-dicom peer; loopback self-tests cover the same paths |
| `image` crate backend | Optional | Not a dependency; PNG/BMP export is covered by the hand-written encoders |
| Single-consumer trait abstractions from fo-dicom | Form | Rust side has concrete equivalents; empty trait shells are deliberately not created |
| C-MOVE against a real peer | Environment | The peer did not map the destination AE to this host; loopback covers the function |

### 3.1 Definition of done (per feature group)

A group is done when:

1. every feature-list item is implemented and unit-tested;
2. every rule/table row of its architecture section has a passing test (tolerance rows included; every
   T1–T10 row has a test named after it);
3. round-trip and golden tests are updated and green;
4. the milestone status above is updated;
5. [01-architecture.md](01-architecture.md) is corrected where the implementation deviated from it.

---

## 4. Risks

| # | Risk | Impact | Mitigation |
|---|---|---|---|
| R1 | Tolerance rules are scattered folklore; missing one fails on real data | High | Build the corpus from fo-dicom's issue list; fuzz the reader |
| R2 | Codec boundary churn breaks `radicom-codecs` after the freeze | High | Boundary review with stakeholders; semver discipline |
| R3 | Multi-charset (ISO 2022) handling is intricate and corrupts CJK names | High | Standalone charset module with fo-dicom-aligned tests against real samples |
| R4 | Network detail (async window, circuit breaker, max-PDU interplay) causes interop bugs | Medium | Loopback plus peer tests, PDU goldens |
| R5 | DICOMDIR offset computation and record stitching are error-prone | Medium | Two-pass writer, golden files, offset tolerance tests |
| R6 | Dictionary XML licensing/version drift | Low | Keep the MS-PL attribution; pin the standard version constant |
| R7 | P2 scope creep delaying P0 | Medium | P2 only after M7; status table keeps deferrals visible |

---

## 5. Testing

### 5.1 Principles

1. **The tolerance corpus is a first-class deliverable**: every lenient behaviour (T1–T10, DICOMDIR
   nearest offset, JSON tolerance) has a named test referencing the originating issue or standard clause.
2. **Round-trips are the backbone**: read → write → re-read must be byte- or semantics-equal across the
   syntax matrix.
3. **Interop, not just self-consistency**: network paths are tested against external peers.
4. **No-panic guarantee**: parse and network decode paths are fuzzed; all failures are typed errors.
5. **A test must be able to fail.** If reverting the fix leaves the test green, it is not covering the
   defect. Verification for both directions of a bug fix is part of the definition of done.

### 5.2 Organization

```
crates/radicom/src/**/tests.rs   # unit tests beside the code
crates/radicom/tests/            # integration: corpus, goldens, fuzz
crates/radicom-network/tests/    # loopback, goldens, fuzz, interop
crates/radicom-json/tests/       # JSON goldens
tests-data/                      # shared fixtures with a provenance README
```

Naming: `<area>_<behavior>__<case>`, with the rule ID suffix for tolerance tests (e.g.
`reader_un_vr_sq_recovery__T3`).

### 5.3 Test data

A 23-sample corpus (22 `.dcm` files plus a `DICOMDIR`) from fo-dicom's test data lives in `tests-data/`
under the fo-dicom **MS-PL** license with a provenance note. Categories covered: syntax matrix
(explicit/implicit LE, explicit BE, deflated), encapsulated (RLE plus a JPEG baseline), malformed corpus
(one per T1–T10 row, synthesized where no fixture exists), multi-frame, charsets (CJK ISO 2022, GBK,
GB18030, UTF-8), structures (nested SQ, fragments, private tags), media (DICOMDIR), SR, meta variants (no
preamble / no meta / ACR-NEMA), and golden PDU byte captures.

### 5.4 Round-trip and goldens

- **Byte-level round-trip**: parse → write with identical options → byte-equal.
- **Semantic round-trip**: where byte equality is impossible (group length removal, padding), compare via
  dataset deep equality.
- **Goldens**: JSON output, DICOMDIR offsets, dump strings, PDU byte streams — generated once, reviewed,
  then enforced. Regenerate with `cargo xtask regen-goldens`; CI fails on drift.

### 5.5 Network testing

- **Loopback**: SCU↔SCP integration for echo/store/find/get/move, lifecycle, retry/linger/breaker with a
  scripted mock peer.
- **Reference interop** (optional runner): against DCMTK `storescp`/`storescu`/`echoscu` and a fo-dicom
  host. Gated on `RADICOM_PEER`; otherwise the tests self-skip.
- **Fuzz**: mutated PDU streams must produce typed errors without panics or unbounded reads.

### 5.6 Codec testing

Only one configuration exists today: the empty registry plus the test-only mock codecs in
`pixel/codec.rs` and `pixel/transcoder.rs`, which validate the facade, the transcoder decision table, the
missing-codec error paths and the boundary contract itself.

A second configuration — end-to-end transcoding against the real `radicom-codecs` codecs — is added as an
optional dev-dependency once that project exists. Nothing in this workspace references it today, so this
repository never hard-depends on it.

### 5.7 Fuzzing

Pure `std`, deterministic, no `cargo-fuzz` needed: `crates/radicom/tests/fuzz.rs` sweeps arbitrary bytes,
byte-mutated valid files and a bogus ~4 GiB declared length through the parser;
`crates/radicom-network/tests/fuzz.rs` sweeps arbitrary PDU streams through `pdu::decode`. Everything must
return a typed `Result` without panicking. Memory guarantees: on-demand windows never load whole values,
and C-STORE receive is bounded by spooling.

### 5.8 Benchmarks

`criterion` is not vendored in the offline cache, so the suites are dependency-free `harness = false`
binaries using `std::time::Instant`:

| Binary | Measures | Run with |
|---|---|---|
| `crates/radicom/benches/core.rs` | parse_dataset, write_dataset, get_frame, render_frame | `cargo bench -p radicom --all-features` |
| `crates/radicom-json/benches/json.rs` | dataset_to_json, json_to_dataset, json_roundtrip | `cargo bench -p radicom-json` |
| `crates/radicom-network/benches/throughput.rs` | loopback C-STORE MiB/s | `cargo bench -p radicom-network` |

Note: `core.rs` benchmarks `from_bytes`; the file `open()` path is measured separately because on-demand
windows make it a different code path.

### 5.9 CI stages

| Stage | Trigger | Content |
|---|---|---|
| clippy | push to `master`/`main`, pull request, `workflow_dispatch` | clippy `-D warnings` |
| test | push to `master`/`main`, pull request, `workflow_dispatch` | unit + integration, default and all-features |
| msrv | push to `master`/`main`, pull request, `workflow_dispatch` | MSRV (1.89) build |
| docs | push to `master`/`main`, pull request, `workflow_dispatch` | rustdoc build |
| package | push to `master`/`main`, pull request, `workflow_dispatch` | `cargo package --list` metadata/license checks |
| fuzz | local/manual | both fuzz targets |
| interop | local / self-hosted | DCMTK / fo-dicom matrix |
| bench | local/manual | `cargo bench` against a baseline |
| coverage | local/manual | `cargo llvm-cov` with `llvm-tools-preview`; current production line coverage **90.03%** (core 90.19%, network 91.30%, json 76.76%) |

### 5.10 Conventions

- Update the milestone status and any changed architecture statement in the same commit as the code.
- Record exit-condition verification (test summary or interop notes) with the milestone.
- Never weaken a test to make it pass; fix the cause.
