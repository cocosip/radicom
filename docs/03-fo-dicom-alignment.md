# fo-dicom Alignment Audit (Module-Level, Code-Walkthrough)

> Current defect and fix evidence: [05-code-review-2026-11-02.md](05-code-review-2026-11-02.md).
> "DONE" below is verified by a test that would fail if the behaviour regressed; it is not a claim of
> complete behavioural equivalence with fo-dicom.

A module-by-module verdict on how far `radicom` matches fo-dicom's feature surface, answering
"what is done and what is not" **from reading the code**, not from self-reported status.

## Method and evidence standard

- Self-reported status is not trusted: each module's symbols, implementation and tests were read, and
  confirmed with `cargo test --workspace --all-features` and
  `cargo clippy --workspace --all-features --all-targets`.
- Verdict definitions:
  - **DONE** — a real implementation, behaviour tests, and a complete end-to-end data flow (not just
    fields or tag constants).
  - **Remaining: scope** — depends on a separate project outside this repository (`radicom-codecs`).
  - **Remaining: environment** — needs an external dependency or peer this machine does not have.
  - **Remaining: platform** — needs FFI on a platform other than this host (macOS).
  - **Not needed** — fo-dicom relies on a .NET-native facility with no Rust equivalent; forcing one in
    would add a dependency for no benefit.

## Verdict

The library implements the broad in-scope feature surface using Rust ownership, enums, explicit
registries and async transport. Concrete codecs remain out of scope; real external peers, print/media
hardware and other operating systems need separate evidence. The 2026-11-02 fresh review found 44
defects behind earlier "DONE" claims (one PHI leak, three P1s) — all are now fixed with regression tests;
see [05-code-review-2026-11-02.md](05-code-review-2026-11-02.md) for the itemised evidence.

One correction is worth recording: the generated UID dictionary was always right, but the hand-written
transfer-syntax table had wrong HTJ2K keys. It now uses generated UID constants — HTJ2K `.4.201/.4.202/.4.203`
(JPIP HTJ2K `.4.204/.4.205`), HEVC Main 10 `.4.108`, JPEG XL Lossless `.4.110`, SMPTE ST 2110 `.7.1/.7.2`.
`crates/radicom/tests/fixtures/fo-dicom-transfer-syntaxes.tsv` pins the 56 registered entries, including
names and encoding flags, against fo-dicom commit
[`d3e6b48`](https://github.com/fo-dicom/fo-dicom/tree/d3e6b48426866fa7bf5a52414acca4898f73b6da).

## Done (with code evidence)

| Module group | Features | Evidence (file / symbol) |
|---|---|---|
| Identifiers (TAG/VR/UID/TS/DICT/VM) | TAG-01..05, VR-01..02, UID-01..05, TS-01..03, DICT-01..03, VM-01 | `model/{tag,masked_tag,tag_index,vr,uid,transfer_syntax,dictionary,vm}.rs`; `network/print_sop.rs` (UID-04); `Dictionary::load_xml` + default replacement (DICT-03); `TransferSyntax::register` (TS-02) |
| Dataset / elements (DS/ELEM/MATCH/XFORM) | DS-01..10, ELEM-01..05, MATCH-01, XFORM-01 | `model/dataset.rs` (`get_*`/`try_get_*`/`insert`/`set_validation`/`functional_groups`/`DatasetWalker`/`copy_to`/`get_datetime`/`enumerate_group`/`enumerate_masked`), `model/element.rs` (DS comma tolerance "1,5"→1.5), `rules/{match_rule,transform}.rs` |
| Charsets / validation (ENC/VAL) | ENC-01..04, VAL-01..03 | `encoding/charset.rs` (`register_charset` + `charset_for_encoding`), `validation/mod.rs` (global switch + per-thread `ValidationScopeGuard`) |
| File IO (FILE/RD/WR/BUF/IO/MEM) | FILE-01..06, RD-01..05, WR-01..03, BUF-01..02, IO-01..03, MEM-01 | `io/{file,reader,writer,dataset_builder,byte_source,file_ref,pool}.rs`; `LargeValuePolicy` (FILE-02), `has_valid_header` (FILE-06), `item_len/header_len` (WR-02), deflate (RD-04) |
| Pixel / codec interfaces (PIX/CODEC/TR) | PIX-01..05, CODEC-01..03,05, TR-01..03 | `pixel/{mod,codec,overlay,photometric,transcoder}.rs`; `uncompressed_frame_size` (PIX-03), palette LUT (PIX-04), `CodecRegistry`/`CodecParams`/JPEG probing (CODEC), `Transcoder` + `Dataset/DicomFile::clone_with_syntax` (TR-03) |
| Rendering (REND) | REND-01..11 | `render/{mod,bit_depth,color_table,geometry,icon,interpolation,math,pixel_converter,reconstruction,spatial}.rs`; all-PI dispatch, modality/VOI/palette LUTs, `GrayscaleRenderOptions`, `BitDepth`, `PixelDataConverter`, spatial (scale/rotate/flip/pan), MPR, math (Matrix/Histogram/BitList/MovingAverage/Rect/Point2), **hand-written dependency-free PNG/BMP** (`to_png`/`to_png_rgb`/`to_bmp`/`save`) |
| Network (NET/SRV/CLI/HOST/DIMSE/OBS) | NET-01..08, SRV-01..08, CLI-01..02, HOST-01, DIMSE-01, OBS-01/02 | `radicom-network/src/*.rs`; C-ECHO/STORE/FIND/MOVE/GET, N-services, C-CANCEL (`client.cancel` and the independent cancellation handle), PDV assembly (`PduSplitter`/`split_message`), extended negotiation (`extended.rs`), TLS (`tls.rs`), hosting (`hosting.rs`), `observability.rs` (OBS-01 plus `TracingMetrics` under the `otel` feature, no `opentelemetry` dependency) |
| Serialization (JSON/XML) | JSON-01..02, XML-01 | `radicom-json/{lib,xml}.rs` (NaN/Inf as strings, bidirectional BulkDataURI, PN three groups, XML NativeDicomModel shape) |
| Media (MEDIA) | MEDIA-01..03 | `media/{mod,icon,scanner}.rs` (DICOMDIR read/write, icon generation, directory scanning) |
| Structured reports (SR) | SR-01..02 | `sr/{content_item,code_item,structured_report}.rs` (15 value types including SCOORD3D, 7 relationships, MeasuredValue/ReferencedSop, build + write-back + traversal + open/save) |
| Printing (PRINT) | PRINT-01..02 | `print/{models,layout,render}.rs` (`initialize_image_boxes`, `render_to_image`) |
| Anonymization (ANON) | ANON-01 | `anonymizer/{mod,profile}.rs` (full PS3.15 table, 11 composable options, sequence recursion, consistent UID replacement) |
| Infrastructure (INF) | INF-01..07 | `error.rs`, `logging.rs`, `dump/mod.rs`, `bin/radicom-dump.rs`, `xtask/src/main.rs` (code generation) |

## Not done / out of repository

| Item | Category | Reason / current state |
|---|---|---|
| JPEG / JPEG-LS / JPEG 2000 / HTJ2K / RLE implementations | Scope | Belongs to the external `radicom-codecs` project by architecture (this repository defines only the `Codec` trait, `CodecParams`, the `CodecRegistry` injection point and the `Transcoder` flow) |
| UID-05 macOS MAC reading | Platform | Needs CoreWLAN/IOKit FFI (Windows `GetAdaptersAddresses` and Linux are implemented and tested) |
| Reference-peer (DCMTK / fo-dicom) network regression | Environment | Needs an external real peer; local loopback self-tests cover the same paths |
| `image` crate backend | Optional | Not added; PNG/BMP export is covered by the hand-written encoders, so no capability is missing |
| `IPixelData`/`Image` traits, `InvertLut`/`PaddingLut`/`CompositeLut`/`PrecalculatedLut`, `CacheType` | Shape | Single-implementation / no-outer-consumer abstractions from fo-dicom's internals; Rust already has concrete equivalents (`RawImage`/`BitDepth`/`ColorTable`) |
| Additional OBS integrations | Not needed | Covered by OBS-01 (`MetricsCollector`) plus OBS-02 (`TracingMetrics` under `otel`, no `opentelemetry` dependency) |
| Print CUBIC / LIN optical-density models | Feature | Not implemented; print rendering uses a linear density window |

## Known limits (not defects)

- The stateless compressed renderer decodes the full dataset once per render (no decoded-frame cache).
- Client pipelining covers C-ECHO batches (at most 64 outstanding); other request kinds are sequential.
  The server executes requests serially within a bounded negotiated window — usable async-window
  support, not full mixed-service multiplexing equivalence.
- Receive-side dataset assembly materializes a complete dataset (1 GiB per message); send-side streaming
  does not imply streaming receive.
- The read-side PDU cap is 256 KiB and applies to A-ASSOCIATE too. PS3.8's Max-PDU-Length formally
  governs only P-DATA-TF; the cap is a deliberate allocation-safety deviation.
- Biplane XA/XRF (`1.2.840.10008.5.1.4.1.1.12.3`) is absent from the modality-LUT skip list because it
  has no generated UID constant; add it through the dictionary generator, then extend `is_xa_xrf`.
- An external private SQ written with a *defined* length under Implicit VR still needs a private
  dictionary to recover its structure (its bytes are preserved).
- Concrete codecs, external peers, print/media hardware and non-Windows evidence are not established by
  local tests.

## Documented divergences from fo-dicom

These are deliberate, tested differences rather than missing features.

| Area | Divergence | Why |
|---|---|---|
| JSON `NumberMode` default | `JsonOptions::default()` uses `PreferablyAsNumber`, not fo-dicom's `AsNumber`. An IS/DS value is written as a JSON number only when the number's own text form is identical, so `"0001"` stays a string. | fo-dicom writes `1`, which loses the stored text; `NumberMode::AsNumber` reproduces fo-dicom exactly when byte-compatible output is required. |
| MIME/XML transfer-syntax retirement | The two retired syntaxes retain `is_retired = true` from the standard UID registry. | The reference fo-dicom transfer-syntax initializers omit this flag, although its UID dictionary marks both retired. |
| Streaming C-STORE dataset | The dataset is streamed through the writer as P-DATA PDUs (one PDV per PDU) instead of one buffered `Vec<u8>`; a transcoded C-STORE still materialises its output. | Peak memory is O(max_pdu + `large_object_size` + a couple of PDUs); the reassembled message is identical, only PDU framing differs. |
| JSON `BulkDataURI` for deferred buffers | A deferred value (`FileWindow`/`StreamWindow`/`TempFileSpill`) is written as `BulkDataURI` only when `JsonOptions::bulk_data_uri` names one; otherwise it is materialised and base64-encoded. | Naming a URI means hosting the bytes, which is the caller's job; the default keeps output byte-compatible with fo-dicom's converter. |
| Anonymizer options | `AnonOptions` is a bitmask; a single `AnonOption` implies `BasicProfile + it`, and enabled columns override the Basic action in column order. | Matches fo-dicom's `[Flags] SecurityProfileOptions` + `LoadProfile`; a single exclusive column would leave PHI behind. |

## Reproducing

```pwsh
cargo test --workspace --all-features
cargo clippy --workspace --all-features --all-targets
cargo run --package xtask -- coverage   # production-line coverage, 95% floor
```

> Caveat: this audit checks feature presence and code paths. It is **not** an adversarial correctness
> review against real-world data; use the corpus and interoperability suites for that.
