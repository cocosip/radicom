# fo-dicom Alignment Audit (Module-Level, Code-Walkthrough)

> Current correction and evidence ledger: [07-review-fixes-2026-10-30.md](07-review-fixes-2026-10-30.md).
> Earlier DONE / coverage statements describe an earlier checkpoint, not proof of complete parity.

A module-by-module verdict on how far `radicom` matches fo-dicom's feature surface, answering
"what is done and what is not" **from reading the code**, not from self-reported status.

## Method and Evidence Standard

- Self-reported status is not trusted: each module's symbols, implementation and tests were read, and the
  findings were confirmed by running `cargo test --workspace --all-features` and
  `cargo clippy --workspace --all-features --all-targets`.
- Verdict definitions:
  - **DONE** — a real implementation, behaviour tests, and a complete end-to-end data flow (not just
    fields or tag constants).
  - **Remaining: scope** — depends on a separate project outside this repository (e.g. `radicom-codecs`).
  - **Remaining: environment** — needs an external dependency or peer this machine does not have.
  - **Remaining: platform** — needs FFI on a platform other than this host (e.g. macOS).
  - **Not needed** — fo-dicom relies on a .NET-native facility with no Rust equivalent; forcing one in
    would add a dependency for no benefit.

## Verdict

The library implements the broad in-scope feature surface using Rust ownership, enums, explicit
registries and async transport. This is not a claim of complete fo-dicom behavioral equivalence.
The 2026-10-30 review found defects behind prior DONE labels; the current repair/evidence matrix is
[07-review-fixes-2026-10-30.md](07-review-fixes-2026-10-30.md). Concrete codecs remain excluded.
Real external peers, print/media hardware and other operating systems need separate evidence.

A second, adversarial review on 2026-10-29 found real parity defects behind previously "DONE" claims
(wrong functional-group tags, a missing FileMetaInformationVersion, dropped ISO 2022 designators, the
Icon Image Sequence tag, multi-fragment single-frame data, an SR anonymization PHI leak, and several
network wire mismatches). Those are fixed with regression tests; see
[05-code-review-2026-10-29.md](05-code-review-2026-10-29.md). "DONE" below now means the behaviour is
verified by a test that would have failed before that review.

## Done (with code evidence)

| Module group | Features | Evidence (file / symbol) |
|---|---|---|
| Identifiers (TAG/VR/UID/TS/DICT/VM) | TAG-01..05, VR-01..02, UID-01..05, TS-01..03, DICT-01..03, VM-01 | `model/{tag,masked_tag,tag_index,vr,uid,transfer_syntax,dictionary,vm}.rs`; `network/print_sop.rs` (UID-04); `Dictionary::load_xml` + default replacement (DICT-03); `TransferSyntax::register` (TS-02) |
| Dataset / elements (DS/ELEM/MATCH/XFORM) | DS-01..10, ELEM-01..05, MATCH-01, XFORM-01 | `model/dataset.rs` (`get_*`/`try_get_*`/`insert`/`set_validation`/`functional_groups`/`DatasetWalker`/`copy_to`/`get_datetime`/`enumerate_group`/`enumerate_masked`), `model/element.rs` (DS comma tolerance "1,5"→1.5), `rules/{match_rule,transform}.rs` |
| Charsets / validation (ENC/VAL) | ENC-01..04, VAL-01..03 | `encoding/charset.rs` (`register_charset` + `charset_for_encoding`), `validation/mod.rs` (global switch + `ValidationScopeGuard`) |
| File IO (FILE/RD/WR/BUF/IO/MEM) | FILE-01..06, RD-01..05, WR-01..03, BUF-01..02, IO-01..03, MEM-01 | `io/{file,reader,writer,dataset_builder,byte_source,file_ref,pool}.rs`; `LargeValuePolicy` (FILE-02), `has_valid_header` (FILE-06), `item_len/header_len` (WR-02), deflate (RD-04) |
| Pixel / codec interfaces (PIX/CODEC/TR) | PIX-01..05, CODEC-01..03,05, TR-01..03 | `pixel/{mod,codec,overlay,photometric,transcoder}.rs`; `uncompressed_frame_size` (PIX-03), palette LUT (PIX-04), `CodecRegistry`/`CodecParams`/JPEG probing (CODEC), `Transcoder` + `Dataset/DicomFile::clone_with_syntax` (TR-03) |
| Rendering (REND) | REND-01..11 | `render/{mod,bit_depth,color_table,geometry,icon,interpolation,math,pixel_converter,reconstruction,spatial}.rs`; all-PI dispatch, modality/VOI/palette LUTs, `GrayscaleRenderOptions`, `BitDepth`, `PixelDataConverter`, spatial (scale/rotate/flip/pan), MPR, math (Matrix/Histogram/BitList/MovingAverage/Rect/Point2), **hand-written dependency-free PNG/BMP** (`to_png`/`to_png_rgb`/`to_bmp`/`save`) |
| Network (NET/SRV/CLI/HOST/DIMSE/OBS) | NET-01..08, SRV-01..08, CLI-01..02, HOST-01, DIMSE-01, OBS-01/02 | `radicom-network/src/*.rs`; C-ECHO/STORE/FIND/MOVE/GET, N-services, C-CANCEL (`client.cancel` and the independent cancellation handle), PDV assembly (`PduSplitter`/`split_message`), extended negotiation (`extended.rs`), TLS (`tls.rs`), hosting (`hosting.rs`), `observability.rs` (OBS-01 plus `TracingMetrics` under the `otel` feature, no `opentelemetry` dependency) |
| Serialization (JSON/XML) | JSON-01..02, XML-01 | `radicom-json/{lib,xml}.rs` (NaN/Inf as strings, bidirectional BulkDataURI, PN three groups, XML NativeDicomModel shape) |
| Media (MEDIA) | MEDIA-01..03 | `media/{mod,icon,scanner}.rs` (DICOMDIR read/write, icon generation, directory scanning) |
| Structured reports (SR) | SR-01..02 | `sr/{content_item,code_item,structured_report}.rs` (15 value types including SCOORD3D, 7 relationships, MeasuredValue/ReferencedSop, build + write-back + traversal + open/save) |
| Printing (PRINT) | PRINT-01..02 | `print/{models,layout,render}.rs` (`initialize_image_boxes`, `render_to_image`) |
| Anonymization (ANON) | ANON-01 | `anonymizer/{mod,profile}.rs` (full PS3.15 table, 11 options, sequence recursion, consistent UID replacement) |
| Infrastructure (INF) | INF-01..07 | `error.rs`, `logging.rs`, `dump/mod.rs`, `bin/radicom-dump.rs`, `xtask/src/main.rs` (code generation) |

## Not Done (stated plainly)

| Item | Category | Reason / current state |
|---|---|---|
| JPEG / JPEG-LS / JPEG 2000 / HTJ2K / RLE implementations | Scope | Belongs to the external `radicom-codecs` project by architecture (this repository defines only the `Codec` trait, `CodecParams`, the `CodecRegistry` injection point and the `Transcoder` flow; the crate is not in the workspace) |
| UID-05 macOS MAC reading | Platform | Needs CoreWLAN/IOKit FFI (Windows `GetAdaptersAddresses` and Linux are implemented and tested; macOS is not this host) |
| Reference-peer (DCMTK / fo-dicom) network regression | Environment | Needs an external real peer; locally loopback self-tests cover the same paths |
| `image` crate backend | Optional | The dependency was **not** added; PNG/BMP export is covered by the hand-written encoders (`to_png`/`to_bmp`), so no export capability is missing |
| `IPixelData`/`Image` traits, `InvertLut`/`PaddingLut`/`CompositeLut`/`PrecalculatedLut`, `CacheType` | Shape | Single-implementation / no-outer-consumer abstractions from fo-dicom's internal architecture; Rust already has concrete equivalents (`RawImage`/`BitDepth`/`ColorTable`), and empty trait shells were deliberately not created |
| Additional OBS integrations | Not needed | Covered by OBS-01 (`MetricsCollector`) plus OBS-02 (`TracingMetrics` under the `otel` feature, emitted through `tracing` field conventions, **no `opentelemetry` dependency**) |

## Documented divergences from fo-dicom

These are deliberate, tested differences rather than missing features.

| Area | Divergence | Why |
|---|---|---|
| JSON `NumberMode` default | `JsonOptions::default()` uses `PreferablyAsNumber`, not fo-dicom's `AsNumber`. An IS/DS value is written as a JSON number only when the number's own text form is identical, so `"0001"` stays a string. | fo-dicom writes `1`, which loses the stored text; `NumberMode::AsNumber` reproduces fo-dicom exactly when byte-compatible output is required. |
| Streaming C-STORE dataset | The dataset is streamed through the writer as P-DATA PDUs (one PDV per PDU) instead of one buffered `Vec<u8>`; a transcoded C-STORE still materialises its output. | Peak memory is O(max_pdu + `large_object_size` + a couple of PDUs), independent of instance size; the reassembled DIMSE message and stream bytes are identical, only the PDU framing differs. |
| JSON `BulkDataURI` for deferred buffers | A deferred value (`FileWindow`/`StreamWindow`/`TempFileSpill`) is written as `BulkDataURI` only when `JsonOptions::bulk_data_uri` names one; otherwise it is materialised and base64-encoded. A URI parsed from JSON is still echoed unchanged. | Naming a URI means hosting the bytes somewhere, which is the caller's job; the default keeps output byte-compatible with the previous release and with fo-dicom's converter. |

## Reproducing

```pwsh
cargo test --workspace --all-features
cargo clippy --workspace --all-features --all-targets
cargo xtask coverage          # production line coverage, 95% floor
```

## Earlier Test Baseline (superseded by the review ledger)

14 test targets green: **1,374 tests, 1 ignored, 0 failures**; clippy and rustdoc clean; production
line coverage **95.62%** (core 95.77%, json 96.03%, network 95.20%). Breakdown: core lib 825, dump
binary 5, cli_dump 1, corpus 7, core fuzz 3, core goldens 1, radicom-dict 5, radicom-json lib 76, JSON
goldens 1, radicom-network lib 434, network fuzz 2, network goldens 3, interop 2 run + 1 ignored,
xtask 6, peer_interop example 1.

> Caveat: this audit checks feature presence and code paths. It is **not** an adversarial correctness
> review against real-world data; use the corpus and interoperability suites for that.
