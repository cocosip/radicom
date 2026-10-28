# fo-dicom Alignment Audit (Module-Level, Code-Walkthrough)

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

**All P0/P1 and the P2 core features are DONE.** The only incomplete items are the concrete codec
implementations (the external `radicom-codecs` project), macOS MAC reading (platform), reference-peer
interop (environment), and a handful of single-consumer / no-outer-consumer shape abstractions that
deliberately were not created. Observability is implemented through `tracing` field conventions and
introduces **no `opentelemetry` dependency**.

## Done (with code evidence)

| Module group | Features | Evidence (file / symbol) |
|---|---|---|
| Identifiers (TAG/VR/UID/TS/DICT/VM) | TAG-01..05, VR-01..02, UID-01..05, TS-01..03, DICT-01..03, VM-01 | `model/{tag,masked_tag,tag_index,vr,uid,transfer_syntax,dictionary,vm}.rs`; `network/print_sop.rs` (UID-04); `Dictionary::load_xml` + default replacement (DICT-03); `TransferSyntax::register` (TS-02) |
| Dataset / elements (DS/ELEM/MATCH/XFORM) | DS-01..10, ELEM-01..05, MATCH-01, XFORM-01 | `model/dataset.rs` (`get_*`/`try_get_*`/`insert`/`set_validation`/`functional_groups`/`DatasetWalker`/`copy_to`/`get_datetime`/`enumerate_group`/`enumerate_masked`), `model/element.rs` (DS comma tolerance "1,5"→1.5), `rules/{match_rule,transform}.rs` |
| Charsets / validation (ENC/VAL) | ENC-01..04, VAL-01..03 | `encoding/charset.rs` (`register_charset` + `charset_for_encoding`), `validation/mod.rs` (global switch + `ValidationScopeGuard`) |
| File IO (FILE/RD/WR/BUF/IO/MEM) | FILE-01..06, RD-01..05, WR-01..03, BUF-01..02, IO-01..03, MEM-01 | `io/{file,reader,writer,dataset_builder,byte_source,file_ref,pool}.rs`; `LargeValuePolicy` (FILE-02), `has_valid_header` (FILE-06), `item_len/header_len` (WR-02), deflate (RD-04) |
| Pixel / codec interfaces (PIX/CODEC/TR) | PIX-01..05, CODEC-01..03,05, TR-01..03 | `pixel/{mod,codec,overlay,photometric,transcoder}.rs`; `uncompressed_frame_size` (PIX-03), palette LUT (PIX-04), `CodecRegistry`/`CodecParams`/JPEG probing (CODEC), `Transcoder` + `Dataset/DicomFile::clone_with_syntax` (TR-03) |
| Rendering (REND) | REND-01..11 | `render/{mod,bit_depth,color_table,geometry,icon,interpolation,math,pixel_converter,reconstruction,spatial}.rs`; all-PI dispatch, modality/VOI/palette LUTs, `GrayscaleRenderOptions`, `BitDepth`, `PixelDataConverter`, spatial (scale/rotate/flip/pan), MPR, math (Matrix/Histogram/BitList/MovingAverage/Rect/Point2), **hand-written dependency-free PNG/BMP** (`to_png`/`to_png_rgb`/`to_bmp`/`save`) |
| Network (NET/SRV/CLI/HOST/DIMSE/OBS) | NET-01..08, SRV-01..08, CLI-01..02, HOST-01, DIMSE-01, OBS-01/02 | `radicom-network/src/*.rs`; C-ECHO/STORE/FIND/MOVE/GET, N-services, C-CANCEL (`client.cancel`), PDV assembly (`PduSplitter`/`split_message`), extended negotiation (`extended.rs`), TLS (`tls.rs`), hosting (`hosting.rs`), `observability.rs` (OBS-01 plus `TracingMetrics` under the `otel` feature, no `opentelemetry` dependency) |
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

## Reproducing

```pwsh
cargo test --workspace --all-features
cargo clippy --workspace --all-features --all-targets
```

## Current Test Baseline

17 test suites green: 808 tests total (core 498, network 248, json 34, dict 4, corpus 5, plus the
integration suites), 0 failures; clippy clean.

> Caveat: this audit checks feature presence and code paths. It is **not** an adversarial correctness
> review against real-world data. The differential review against fo-dicom found defects that these tests
> do not catch — see [superpowers/00-executive-summary.md](superpowers/00-executive-summary.md).
