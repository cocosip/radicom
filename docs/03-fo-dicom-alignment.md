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
- Two independent code-walkthroughs feed this record: the parity gap audit (42 findings, IDs H/M/L
  below) and the fresh review (44 findings, [05](05-code-review-2026-11-02.md)). Neither exercised a
  real-world corpus or an external peer; both are source-level audits.
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

The parity gap audit that preceded it found 42 behavioural gaps (5 High, 16 Medium, 21 Low); 41 are
fixed and L5 is dependency-gated (PFX/PKCS#12 import and OS revocation). Every fixed item carries a
regression test; the per-ID record is below.

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
- Client batching covers C-ECHO, C-STORE and C-FIND (at most `min(invoked, 64)` outstanding); C-GET,
  C-MOVE and N-ACTION stay sequential. The SCP dispatches C-ECHO/C-STORE/C-FIND/N-services (except
  N-ACTION) to tasks but serialises each response through one writer, bounded by the AC window.
- Receive-side C-STORE PDVs spill to a temporary file (64 GiB default bound) before parsing, but the
  parse still materialises the dataset once (the core exposes only byte-slice `parse_dataset_bytes`),
  and the SCU reverse-store (C-GET) path still buffers. Send-side streaming does not imply a
  file-backed dataset parse.
- The read-side PDU cap is 256 KiB and applies to A-ASSOCIATE too. PS3.8's Max-PDU-Length formally
  governs only P-DATA-TF; the cap is a deliberate allocation-safety deviation.
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
| Anonymizer options | `AnonOptions` is a bitmask. A single `AnonOption` implies `BasicProfile \| it`; for a matching row the highest enabled column with a non-empty cell wins, and the first matching row wins (`SecurityProfile.LoadProfile` + `FirstOrDefault`). With the baseline enabled the leading odd-private row therefore shadows the `CleanGraph` 50xx/60xx rows; request the column alone with `AnonOptions::bit(..)` for the options-only fo-dicom call. | Matches fo-dicom's `[Flags] SecurityProfileOptions`, its CSV merge order and its regex-row precedence; a single exclusive column would leave PHI behind, hence the baseline implication. |

## Parity gap audit and fix status (2026-11-02)

41 of 42 fixed, 1 partial (L5). Every fixed item carries a regression test; the
full workspace suite (`cargo test --workspace --all-features`), clippy and rustdoc are green.
"Open" means not attempted, not a rejected finding.

| ID | Status | What landed / what remains |
|---|---|---|
| H1 | Fixed | `ReadOptions::fallback_encoding` + `DicomFile::open_with_encoding`, dataset fallback list used by the parser and the writer, and network `ServerOptions::fallback_encoding` (a DICOM defined term, applied to received C-STORE/C-FIND/C-MOVE/C-GET/N datasets). |
| H2 | Fixed | `DicomClient::send_n_service`/`send_n_action` (all six N services), `NEventReportHandler` + `intercept_event_report` on the six SCU read loops, `ServiceProvider::on_send_n_event_report` for SCP-pushed reports on the same association. Caveat: the peer must hold the SCP role for the class (a role selection replaces the default). |
| H3 | Fixed | C-STORE PDVs spill to a temporary file when the command is decoded (`SpoolConfig`/`SpoolFile`/`MessageBody`), default memory threshold 0, 64 GiB spool bound, sink adopts the file. Ceilings: the dataset parse still materializes once (core offers only `parse_dataset_bytes`), and the SCU reverse-store (C-GET) path still buffers. |
| H4 | Fixed | Per-frame `(0028,9145)` rescale + VOILUTFunction now drive window/level through the flattened functional-group helper; top-level pair is fallback only. |
| H5 | Fixed | `AnonOptions` bitflags (1..1024) with fo-dicom's merged lookup (first matching row, highest enabled column with a cell). A single `AnonOption` implies `BasicProfile \| it` (FJ-01), so a single-option call cannot drop the baseline; explicit single-column calls use `AnonOptions::bit(..)`. |
| M1 | Fixed | Skipped large values/fragment sequences are withheld entirely (`ParseObserver::on_fragment_skipped`); the dead `Skip => EmptyBuffer` arm removed. |
| M2 | Fixed | `write_stream` is the single writer path; `save_with` streams into a sibling temp file and renames, `write` streams to the caller, `to_bytes` wraps the same code. `from_reader` drops the `Seek` bound and spools above `ReadOptions::stream_spool_threshold` (8 MiB default) to a temp file parsed with file windows. Ceiling: large-value windows over a generic `Read+Seek` still need the `ByteSource` refactor, so a seekable non-file source above the threshold pays one temp-file copy. |
| M3 | Fixed | `validate()` is unsuppressible; `insert`/`try_insert_or_update` plus `try_insert_value`/`try_insert_value_with_vr`/`try_insert_str`/`try_insert_empty` validate before mutation and return `Error::Validation`. The infallible helpers are documented lenient channels (the `NotValidated()` equivalent). |
| M4 | Fixed | AT and multi-valued numeric conversions both directions (`Tag`, `Vec<Tag>`, `Vec<u16/i16/u32/i32/u64/i64/f32/f64>`). |
| M5 | Fixed | `get_datetime_with_offset_in(Some(top), …)` reads `(0008,0201)` from the top level; default is machine-local (DST gap/ambiguity handled). |
| M6 | Fixed | Client batch pipeline up to `min(invoked, 64)`; the SCP dispatches C-ECHO/C-STORE/C-FIND/N-services (except N-ACTION) to tokio tasks and serialises whole responses through one writer, bounded by the AC window (cap 64). C-GET/C-MOVE/N-ACTION remain serialized (documented). |
| M7 | Fixed | `ClientOptions::connect_timeout` (default `None`) + `NetworkError::ConnectTimedOut`. |
| M8 | Fixed | `ServerOptions::request_timeout` (default `None`), applied to reverse C-STORE and N-EVENT-REPORT response waits. |
| M9 | Fixed | `ImageBox::new` creates a Basic Grayscale/Basic Color Image Box SOP instance; `FilmBox` keeps the instances and writes `(2010,0510)` references with `(0008,1150)`/`(0008,1155)`; legacy dataset-nested boxes still parse. |
| M10 | Fixed | `FilmSession` registers Presentation LUT SOP instances; `FilmBox` emits/parses `(2050,0500)` and resolves it (`resolve_presentation_lut`); `render_to_image_with_session` applies it. |
| M11 | Fixed | `VolumeData::from_datasets` expands `NumberOfFrames` (and `from_dataset`); `ImageData::from_dataset_frame`/`read_grayscale` read the requested frame. |
| M12 | Fixed | MFSC 1-bit frames reverse bit order; other SOP classes keep LSB-first. |
| M13 | Fixed | DICOMDIR records copy `(0008,0005)`; non-ASCII names round-trip. |
| M14 | Fixed | `tags_as_keywords` falls back to the 8-hex key for unknown/blank/masked/private tags; no element lost. |
| M15 | Fixed | Keyword-keyed JSON resolves through the dictionary; round-trips. |
| M16 | Fixed | Private creators restored on read (including inside sequences). |
| L1 | Fixed | `UidGenerator::regenerate_dataset` walks top level and sequences. |
| L2 | Fixed | Writer recalculates only group lengths that exist; `recalculate_group_lengths`/`remove_group_lengths` added. |
| L3 | Fixed | `ServerOptions`/`ClientOptions::ignore_unsupported_syntax_change` (default false): on a transcode failure the dataset is sent in its own syntax with PixelData intact. |
| L4 | Fixed | `ClientOptions::common_extended` reaches the wire; the SCP still ignores an incoming item rather than echoing it. |
| L5 | Partial | `TlsOptions::certificate_validator` wraps the WebPki verifier and can override or veto the standard verdict (fo-dicom callback semantics). PFX/PKCS#12 import and OS revocation are dependency-gated ceilings (no deps added). |
| L6 | Fixed | `ClientOptions::metrics` wraps the client stream; `CountingStream`/connection and association guards count and close exactly once. |
| L7 | Fixed | `max_pdvs_per_pdu` (default 16) and `max_command_buffer`/`max_data_buffer` on both option structs feed `PduSplitter::with_limits` (can shrink, never raise). |
| L8 | Fixed | `Association::peer_implementation_class_uid()`/`peer_implementation_version_name()` from the AC user information. |
| L9 | Fixed | `StatusServiceClass::{PrintManagement, MediaCreation}` names from the fo-dicom table. |
| L10 | Fixed | Film-level MagnificationType (BILINEAR default), Illumination/ReflectedAmbientLight/RequestedResolutionID, per-box magnification/density. |
| L11 | Fixed | ROI overlays are filtered out; only Graphics overlays are composited. |
| L12 | Fixed | `DicomImage::set_invert`/`clear_invert`. |
| L13 | Fixed | `.lut` output is the fo-dicom 768-byte planar layout; longer legacy interleaved buffers still read. |
| L14 | Fixed | `Line3`, `Segment3`, `Plane3`, `Slice3`, `Orientation3` plus Point3/Vector3 rotate/reflect/distance/nearest-axis helpers. |
| L15 | Fixed | `is_xa_xrf` also matches the retired XA biplane SOP class. |
| L16 | Fixed | `from_bit_range` maps the range through modality-LUT output/rescale into physical space. |
| L17 | Fixed | JSON reader accepts comments and array-form InlineBinary. |
| L18 | Fixed | `dump_file` walks group 0002 then the dataset; the CLI shows both. |
| L19 | Fixed | Dump lines carry element byte length, UID registry names and per-fragment sizes. |
| L20 | Fixed | Parser trace carries element size and dictionary name. |
| L21 | Fixed | `JsonOptions::write_keyword`/`write_name` annotations. |

Batches 1–3 added 60 tests and touch no Cargo manifests; `tests-data/goldens/{GH178,dicomdir}.dump.txt`
were regenerated for the new dump format with `xtask regen-goldens`.

## Reproducing

```pwsh
cargo test --workspace --all-features
cargo clippy --workspace --all-features --all-targets
cargo run --package xtask -- coverage   # production-line coverage, 95% floor
```

> Caveat: this audit checks feature presence and code paths. It is **not** an adversarial correctness
> review against real-world data; use the corpus and interoperability suites for that.
