# fo-dicom Alignment, Gap Audit and Fix Status (Code Walkthrough, 2026-11-02)

> Scope: missing or degraded **functionality** versus fo-dicom, found by reading both codebases.
> Concrete image codecs (JPEG / JPEG-LS / JPEG 2000 / HTJ2K / RLE) are **excluded** by request.
> Reference: fo-dicom commit [`d3e6b48426866fa7bf5a52414acca4898f73b6da`](https://github.com/fo-dicom/fo-dicom/tree/d3e6b48426866fa7bf5a52414acca4898f73b6da)
> (5.2.x line). It is the current parity and fix-status record for this repository.

## Method and evidence standard

- Four module audits (`core model / file IO`, `network`, `imaging / render / print / media`,
  `serialization / SR / anonymizer / logging`) read the fo-dicom source and the radicom source side
  by side. The earlier self-reported alignment verdict was **not** trusted; every claim below was
  checked against code.
- Status: **absent** (no working equivalent), **partial** (exists but a documented capability is
  missing or silent), **inert** (code exists but no reachable consumer applies it).
- Impact: **High** = wrong data / wrong image / a standard workflow cannot be completed;
  **Medium** = capability missing with a workaround or a documented-but-honoured limit;
  **Low** = narrower gap, diagnostic detail, or workaround-only knob.
- The Lead re-read the code for every High finding and most Medium findings; Low findings carry the
  module auditor's citations. The audit itself was a source walkthrough; every fix below carries a
  regression test and the workspace suite is green.
- fo-dicom paths are relative to the fo-dicom repository root, with the `FO-DICOM.Core/` prefix omitted for brevity; radicom paths are relative to this
  repository root.

## Verdict

The model/container layer, the wire protocol, the SR/JSON/XML data mapping, and most of the imaging
pipeline are genuinely at parity: there is **no missing subsystem** (other than the codecs and the
.NET platform backends that are documented divergences). The gaps are specific behavioural holes:

| Area | High | Medium | Low | Summary |
|---|---:|---:|---:|---|
| Core model, file IO | 1 | 5 | 2 | Strong model; file-open contract (charset, streaming, skip policy, write-time validation) lags |
| Network | 2 | 3 | 7 | Wire layer solid; N-service/event direction, receive streaming and timeout knobs lag |
| Imaging / render / print / media | 1 | 5 | 7 | Pipelines present; enhanced-multiframe display and print SOP-instance model lag |
| Serialization / SR / anonymizer / logging | 1 | 3 | 5 | JSON/XML/SR parity+; PS3.15 option sets and JSON keyword/private round trip lag |
| **Total** | **5** | **16** | **21** | 42 confirmed gaps |

Four of the five High findings affect data or a legal standard workflow on inputs that are common
in the field (missing fallback charset, per-frame rescale, option-set anonymization, large-instance
C-STORE); the fifth (N-service/event direction) blocks PS3.7 role-reversed workflows.
The earlier alignment verdict was accurate on the
model and wire layers but overstated completeness for File IO, ANON, JSON keyword handling, network
event reporting and enhanced-multiframe rendering; its "Not Done" table omitted every item below.

## Fix status (2026-11-02, batches 1–3)

41 of 42 fixed, 1 partial (L5). Every fixed item carries a regression test; the
full workspace suite (`cargo test --workspace --all-features`), clippy and rustdoc are green.
"Open" means not attempted, not a rejected finding.

| ID | Status | What landed / what remains |
|---|---|---|
| H1 | Fixed | `ReadOptions::fallback_encoding` + `DicomFile::open_with_encoding`, dataset fallback list used by the parser and the writer, and network `ServerOptions::fallback_encoding` (a DICOM defined term, applied to received C-STORE/C-FIND/C-MOVE/C-GET/N datasets). |
| H2 | Fixed | `DicomClient::send_n_service`/`send_n_action` (all six N services), `NEventReportHandler` + `intercept_event_report` on the six SCU read loops, `ServiceProvider::on_send_n_event_report` for SCP-pushed reports on the same association. Caveat: the peer must hold the SCP role for the class (a role selection replaces the default). |
| H3 | Fixed | C-STORE PDVs spill to a temporary file when the command is decoded (`SpoolConfig`/`SpoolFile`/`MessageBody`), default memory threshold 0, 64 GiB spool bound, sink adopts the file. Ceilings: the dataset parse still materializes once (core offers only `parse_dataset_bytes`), and the SCU reverse-store (C-GET) path still buffers. |
| H4 | Fixed | Per-frame `(0028,9145)` rescale + VOILUTFunction now drive window/level through the flattened functional-group helper; top-level pair is fallback only. |
| H5 | Fixed | `AnonOptions` bitflags (1..1024) with fo-dicom's merged lookup (first row, highest enabled column); the single-option path delegates to the old index, so default BasicProfile output is unchanged. |
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

## High findings

### H1. No fallback character set for files/datasets without (0008,0005)
- **Status:** absent. **Area:** core/IO; also the network receive path.
- **fo-dicom:** `DicomFile.cs:225`, `:281` (`Open(path|stream, Encoding fallbackEncoding, …)`);
  `DicomDataset.cs:154` (`FallbackEncodings`, used by `GetEncodingsForSerialization`);
  `IO/Reader/DicomDatasetReaderObserver.cs:30-43` applies it while decoding.
- **radicom:** no such API. `io/dataset_builder.rs:43` starts every level with
  `charset_encoding("ISO_IR 6")`; `:70-92` decodes only from the dataset's own (0008,0005).
  `ReadOptions` (`io/file.rs:57-72`) has only `read` + `large_object_size`; `open`,
  `open_with_options`, `from_bytes`, `from_reader` expose no encoding. A workspace-wide grep for
  `fallback` finds only the per-segment *encoding* fallback, never a decode default.
- **Impact:** a Part-10 file carrying ISO 8859-1 / Shift-JIS / GBK text without (0008,0005) — common
  in legacy and misbehaving archives — is decoded as ASCII and its PN/LO/LT values become U+FFFD.
  The original bytes are gone, so no caller-side workaround exists. fo-dicom's own tests pin this
  capability (`Tests/FO-DICOM.Tests/DicomFileTest.cs`: `OpenDefaultEncoding` / `OpenUtf8Encoding`).
- **Fix shape:** add a fallback encoding to `ReadOptions` and a dataset-level fallback list, use it in
  `dataset_builder` when (0008,0005) is absent, and expose it on network receive options.

### H2. N-service requests are high-level-client-inaccessible; N-EVENT-REPORT cannot be role-reversed
- **Status:** partial. **Area:** network.
- **fo-dicom:** `Network/Client/DicomClient.cs:213` `OnNEventReportRequest`; `DicomClient` accepts any
  `DicomRequest` (including all N-services); `Network/DicomService.cs:301` `SendRequestAsync` and
  `:1080` `OnSendNEventReportRequestAsync`; `Network/IDicomServiceRunner.cs:12`.
- **radicom:** `client.rs` exposes only C-ECHO/STORE/FIND/MOVE/GET (no N-service); the expert layer
  (`expert.rs:220`) also wraps only C-FIND/MOVE/GET. N-services are reachable only through the
  low-level `server::Association::send_n_service` (`server.rs:2899`). On the SCU side,
  `Association::await_response` (`server.rs:2840`) and `await_n_response` (`server.rs:2944`) ignore
  any command that is not the awaited response, so an incoming N-EVENT-REPORT-RQ is dropped
  unanswered. `Serving` (`server.rs:1410`) is private, so an SCP cannot send a request on its own
  association when role selection granted it the SCU role.
- **Impact:** Storage Commitment Push notifications, MPPS/UPS event reports and print-management
  events cannot use the same-association role-reversal that PS3.7 defines; the high-level client
  cannot even initiate N-ACTION/N-GET/N-SET/N-CREATE/N-DELETE. Workaround: low-level association or a
  separate association plus a server-side provider — workable, but not parity.

### H3. C-STORE receive buffers the whole instance and refuses values above 1 GiB
- **Status:** partial. **Area:** network.
- **fo-dicom:** `Network/DicomService.cs:755-877`: `CreateCStoreReceiveStream(file)` creates a
  `TemporaryFile` and writes each P-DATA PDV straight to it (`:789`); `GetCStoreDicomFile` parses the
  temp file only after the last fragment (`:872`). Both methods are virtual, so a custom sink is a
  first-class extension point.
- **radicom:** `assembler.rs:74` fixes `DEFAULT_MAX_MESSAGE_LENGTH = 1 GiB`; `:111-133` rejects a
  larger message. The store sink runs only after the whole dataset is buffered (`store.rs:78`
  materialises it; `server.rs:1351` receives whole bytes); `SpoolToTemp` copies after buffering.
- **Impact:** peak memory is O(instance) per concurrent store, and a conformant whole-slide/pathology
  instance above 1 GiB is rejected rather than stored. Typical instances (<1 GiB) are unaffected.
  The ceiling was recorded in the earlier review ledger (now in git history), so it was a known
  limit, not a surprise — but it is not fo-dicom behaviour.

### H4. Per-frame rescale (Pixel Value Transformation) is ignored when resolving window/level
- **Status:** partial. **Area:** imaging/render.
- **fo-dicom:** `Imaging/GrayscaleRenderOptions.cs:225-232` and `:295-302` fall back to the flattened
  functional-group `RescaleSlope`/`RescaleIntercept` from `(0028,9145)`; functional groups are
  flattened by `DicomDataset.cs:804-838`.
- **radicom:** `render/mod.rs:274-281` reads only top-level `(0028,1053)`/`(0028,1052)`;
  `from_functional_window_level` (`:467-491`) reads only the per-frame window. The tag constant
  `PIXEL_VALUE_TRANSFORMATION_SEQUENCE` exists but has no consumer outside generated bindings
  (`generated/tags.rs:4844`).
- **Impact:** enhanced MR/CT/PET objects that carry rescale inside the per-frame Pixel Value
  Transformation Sequence render with slope 1 / intercept 0, so the window is applied to stored
  values instead of physical values (a −1024 intercept is lost). fo-dicom's
  `Tests/FO-DICOM.Tests/Imaging/EnhancedRenderingTest.cs:14-56` covers exactly this.
- **Fix shape:** flatten `(0028,9145)` for the frame (the helper already exists at
  `render/geometry.rs:485-519`) and prefer its rescale over the top-level pair; also read per-frame
  VOILUTFunction.

### H5. PS3.15 option sets cannot be combined
- **Status:** partial. **Area:** anonymizer.
- **fo-dicom:** `DicomAnonymizer.cs:33` `SecurityProfileOptions : short` with `[Flags]` (1, 2, 4, …
  1024); `LoadProfile` (`:113-150`) iterates every enabled flag and merges that column's action into
  one profile. The library's own tests use `(SecurityProfileOptions)15`.
- **radicom:** `anonymizer/profile.rs:24-36` is a plain enum (one variant per CSV column);
  `anonymizer/mod.rs:37` stores exactly one `AnonOption`, `:58-66` `with_profile(profile, option)`,
  `:130` `action_for_tag(&tag, self.option)`. No set/mask API exists anywhere in the workspace.
- **Impact:** the default Basic Profile is correct, but `BasicProfile | RetainDeviceIdent |
  RetainPatientChars | …` — the normal PS3.15 combination — cannot be expressed. Callers must
  hand-merge CSV columns into a bespoke profile, losing the option semantics. This is a
  de-identification feature gap, not a cosmetic one.

## Medium findings

### Core model / file IO
| ID | Gap | Evidence (fo-dicom ↔ radicom) |
|---|---|---|
| M1 | `SkipLargeTags` leaves empty elements and empty fragment sequences instead of omitting them | `IO/Reader/DicomReader.cs:648-657` calls the observer only when `GetBuffer` returns non-null; `IO/StreamByteSource.cs:130-134` returns null for the skip policy. radicom `io/reader.rs:349-354` returns `EmptyBuffer::shared()` and `io/dataset_builder.rs:117-124` inserts it; fragments are inserted even when every fragment was skipped (`:208-218`, `io/reader.rs:739-751`). `contains(PixelData)` then says "present but empty", and re-saving writes a zero-length element. |
| M2 | File IO is not streaming: stream input is fully buffered, save materialises the whole file | fo-dicom `IO/StreamByteSource.cs:118-129` turns large values into windows over the caller's stream; `DicomFile.cs:281` opens a stream; `IO/Writer/DicomFileWriter.cs:44-63` writes incrementally. radicom `io/file.rs:225-234` `from_reader` does `read_to_end`; `:273-300` `open_partial` reads the whole file too; `:303-313` `save_with` and `:376-380` `write` build `to_bytes()` first (the writer's streaming path is bypassed at file level); `io/byte_source.rs:20-52` can window only a `std::fs::File`, never an arbitrary `Read+Seek`. `from_reader` also requires `Seek` even though it buffers. |
| M3 | Validation never runs on dataset writes; explicit `validate()` is suppressed by the switches | fo-dicom `DicomDataset.cs:1172,1188,1213` validates every add/update when `ValidateItems` is on, and `:876-886` `Validate()` ignores both switches. radicom `model/dataset.rs:316-347` (typed inserts) and `:562-567` (`validate()` returns `Ok(())` when disabled) make `set_validation` inert for writes: `ds.insert_value(Tag::STUDY_DATE, "not-a-date")` succeeds silently. |
| M4 | No AT or multi-valued numeric element conversions | fo-dicom `DicomElement.cs:711-722` (`Get<DicomTag>` / `DicomTag[]`), `:536-541` (typed arrays for binary VRs), `DicomDataset.cs:930,989` (`Add(tag, params T[])`). radicom `model/element.rs:295-449` has no `FromElement for Tag`; `:851-1050` has no `IntoElement for Tag` or `Vec<u16>/Vec<i32>/Vec<f64>`. Readers work (`Vec<T>` exists); writers must hand-build bytes. Affects FrameIncrementPointer, DimensionIndex pointers/values, LUT descriptors and palette data. |
| M5 | Timezone composition has no top-level lookup and defaults to UTC | fo-dicom `DicomDatasetExtensions.cs:90-126,139-193` reads (0008,0201) from a `topLevelDataset` and defaults to machine-local. radicom `model/dataset.rs:1456-1500` reads only the current dataset and defaults to UTC. Per-frame acquisition date/times in functional groups therefore report `+00:00`. |

### Network
| ID | Gap | Evidence (fo-dicom ↔ radicom) |
|---|---|---|
| M6 | Asynchronous operations window is honoured only for C-ECHO | fo-dicom `Network/DicomService.cs:1145-1146` gates the send loop on `MaxAsyncOpsInvoked` for every request. radicom negotiates the window but only `Association::send_c_echo_batch` (`server.rs:2117`) pipelines; every other `send_*` waits for its response, and the server executes serially. Acknowledged as a limit in the earlier review ledger, so not hidden — but not parity. |
| M7 | No TCP connect timeout | fo-dicom `Network/Client/DicomClientOptions.cs:9` + `Network/DesktopNetworkStream.cs:57`. radicom `server.rs:1947` calls `TcpStream::connect(...).await` with no deadline; `ClientOptions` times association, TLS handshake, request and release only. A blackholed SYN holds the call for the OS retry budget. |
| M8 | No server-side request timeout | fo-dicom `Network/DicomServiceOptions.cs:69` `RequestTimeout`, enforced by `Network/DicomService.cs:1472-1488` for SCP-issued requests too. radicom `ServerOptions` (`server.rs:31-67`) has no request timeout; `Serving::await_store_response` (`server.rs:1539`) reads PDUs with no deadline, so a stalled C-MOVE/C-GET destination wedges the sub-operation loop. |

### Imaging / render / print / media
| ID | Gap | Evidence (fo-dicom ↔ radicom) |
|---|---|---|
| M9 | Basic Film Box → Image Box references and Basic Color Image Box cannot be represented | fo-dicom `Printing/FilmBox.cs:561-590` creates a separate Basic Image Box SOP instance and a `(2010,0510)` item with `(0008,1150)`/`(0008,1155)`; `Printing/ImageBox.cs:39-76` has SOP class/instance and picks `(2020,0110)` vs `(2020,0111)`. radicom `print/models.rs:325-337` writes only `(2020,0010)` ImageBoxPosition inside its own sequence, and `:449-469` always uses `(2020,0110)`; the model has no SOP class/instance fields. The rendered film works, but it is not a conformant Basic Film Box object. |
| M10 | Presentation LUT only as an embedded `(2050,0010)` sequence | fo-dicom `Printing/PresentationLut.cs:14-67`, `FilmBox.cs:351-380` (`ReferencedPresentationLUTSequence` resolved via `FilmSession.FindPresentationLut`), `FilmSession.cs:253-288`. radicom parses `(2050,0010)`/`(2050,0020)` from the film-box dataset (`print/presentation_lut.rs:191-217`, `models.rs:362-373`) but never `(2050,0500)`; an N-CREATE'd standalone Presentation LUT object cannot be linked. |
| M11 | MPR cannot build a volume from a multi-frame dataset | fo-dicom `Imaging/Reconstruction/VolumeData.cs:45,69-76` expands `NumberOfFrames` into `ImageData(dataset, frame)`. radicom `render/reconstruction.rs:36-64` has no frame parameter, `:377-410` always reads `get_frame(0)`, and `VolumeData::from_datasets` (`:124-173`) accepts only one-frame datasets. Enhanced CT/MR single-object series need manual frame splitting. |
| M12 | Multi-frame Single Bit Secondary Capture bit order is not reversed | fo-dicom `Imaging/Render/PixelData.cs:116-131` reverses bits for this SOP class. radicom `render/mod.rs:1217-1230` always unpacks LSB-first; `render/pixel_converter.rs:8-12` defines `reverse_bits` for exactly this case but its only references are the re-export and its own test. Such images display mirrored. |
| M13 | DICOMDIR records do not carry SpecificCharacterSet | fo-dicom `Media/DicomDirectory.cs:719-727` copies (0008,0005) into every record. radicom builds records from fixed attribute lists (`media/mod.rs:549-579`, `:867-882`) and never copies the charset (no `SPECIFIC_CHARACTER_SET` reference under `crates/radicom/src/media`). Non-ASCII PatientName/StudyDescription in a DICOMDIR lose their declared encoding. |

### Serialization / SR / anonymizer
| ID | Gap | Evidence (fo-dicom ↔ radicom) |
|---|---|---|
| M14 | `tags_as_keywords` emits colliding keys and silently drops elements | fo-dicom `Serialization/JsonDicomConverter.cs:116-129` falls back to the hex key for unknown, blank and masked tags. radicom `crates/radicom-json/src/lib.rs:188-196` returns any non-empty keyword; `model/dictionary.rs:73-78` makes every unknown tag `"Unknown"`, `:175-189` makes every creator slot `"PrivateCreator"`, and masked rows (e.g. overlay) reuse one keyword. Values are inserted into a `serde_json::Map` (`lib.rs:164-185`), so later keys overwrite earlier ones. |
| M15 | Keyword-keyed JSON cannot be read | fo-dicom `Serialization/JsonDicomConverter.cs:215-226` falls back to `DicomDictionary[tagstr]`. radicom `radicom-json/src/lib.rs:129-132` calls `Tag::parse` only, and unparsable keys are skipped silently (`:556-560`). radicom's own keyword-mode output therefore reads back as an empty dataset. |
| M16 | Private creators are not restored when reading JSON | fo-dicom `Serialization/JsonDicomConverter.cs:174-187` sets `DicomPrivateCreator` for every private element after reading; its tests assert it. radicom builds a bare tag from the JSON key (`radicom-json/src/lib.rs:548-565`) and `normalize_read` (`model/dataset.rs:542-551`) only resolves creator-slot-shaped elements (≤ 0x00FF), so a block element such as `(0029,1001)` stays creator-less and `get(Tag::private(…, creator))` misses after a round trip. |

## Low findings

### Core
| ID | Gap | Evidence |
|---|---|---|
| L1 | No dataset-wide UID regeneration | fo-dicom `DicomUIDGenerator.cs:43-61` `RegenerateAll(DicomDataset)` rewrites every UI element recursively. radicom `model/uid.rs:398-410` regenerates only the generator's own memoised map and returns `Vec<Arc<Uid>>`; nothing walks a dataset (grep across `crates/`). The name suggests otherwise. |
| L2 | Group-length helper surface absent; `keep_group_lengths` creates instead of recalculating | fo-dicom `IO/Writer/DicomDatasetExtensions.cs:21-101` exposes `RecalculateGroupLength(s)`, `RemoveGroupLengths`; the file writer recalculates only groups that already had a length. radicom `io/writer.rs:112-149` emits a fresh `(gggg,0000)` for every group when the option is on, even if the source had none. Saved bytes gain legacy elements fo-dicom would not write; no in-place normaliser exists. |

### Network
| ID | Gap | Evidence |
|---|---|---|
| L3 | No fallback when the accepted syntax cannot be transcoded | fo-dicom `Network/DicomServiceOptions.cs:64` `IgnoreUnsupportedTransferSyntaxChange` (send as-is or strip PixelData). radicom `server.rs:2358-2370`, `:1665-1673` return an error when no codec is registered. Codec-adjacent; safer than dropping pixels, but a transfer fo-dicom completes fails here. |
| L4 | SOP Class Common Extended Negotiation is inert | coded and validated (`pdu.rs:242,250,541,886`, `extended.rs:272`) but `ClientOptions` has no field and `build_user_information` (`association.rs:564`) leaves it default; every assignment in the workspace is a test. A peer relying on common service class / related general classes sees none. |
| L5 | TLS trust policy is fixed | fo-dicom `Network/Tls/DefaultTlsInitiator.cs:24,34,49` and `Network/Tls/DefaultTlsAcceptor.cs:39,49,66-83` expose validation callbacks, PFX/store loading and revocation. radicom `tls.rs:44-63,252` supports PEM cert/key, mTLS, CA bundle vs platform roots and `accept_any`, but no callback/PFX/revocation. Normal deployments are covered. |
| L6 | Network metrics are server-only | fo-dicom `Network/DicomService.cs:83,123` resolves the metrics collector for every service, client included. radicom `observability.rs:26` is wired only through `ServerOptions.metrics`; nothing attaches a collector to `DicomClient` or the expert connection. |
| L7 | PDU write limits are hardcoded | fo-dicom `Network/DicomServiceOptions.cs:30,33,56` exposes `MaxCommandBuffer`, `MaxDataBuffer`, `MaxPDVsPerPDU`. radicom uses literal `16` PDVs/PDU (`server.rs:481,2086`) and allows one PDV to fill the whole PDU (`assembler.rs:224`). Framing stays valid; the interop workaround knob is missing. |
| L8 | Peer implementation class UID / version name are decoded then dropped | decoded into `pdu.rs:236,499-500` but `Association::open` (`server.rs:1932-2100`) keeps neither; only local constants are used (`server.rs:428`). fo-dicom exposes them on `Network/DicomAssociation.cs:71,76`. |
| L9 | Print-management and media-creation status names are absent | fo-dicom `Network/DicomStatus.cs:512-620`; radicom `dimse.rs:576-706` stops at the generic N/Q/R families, so a print user must register names via `register_status_description` (`dimse.rs:637`). Classifiable, but unnamed. |

### Imaging / render / print / media
| ID | Gap | Evidence |
|---|---|---|
| L10 | Film Box MagnificationType and several print attributes are never read | fo-dicom `Printing/FilmBox.cs:177-181,313-349` (MagnificationType default BILINEAR, Illumination, ReflectedAmbientLight, RequestedResolutionID), `Printing/ImageBox.cs:124-171` (per-box magnification/density). radicom `print/render.rs:160-168` reads only the image box's magnification and requested size; `models.rs:177-374` has no film-level accessors. A film-level `REPLICATE` is ignored. |
| L11 | ROI overlays are composited as graphics | fo-dicom `Imaging/DicomImage.cs:409-412` filters `Type == Graphics`. radicom `render/mod.rs:1486-1507` composites every overlay covering the frame; `pixel/overlay.rs:124-126` exposes the type but it has no consumer. |
| L12 | No runtime Invert control on `DicomImage` | fo-dicom `Imaging/DicomImage.cs:174-193`. radicom derives invert only from `PhotometricInterpretation == "MONOCHROME1"` (`render/mod.rs:308-311`); `DicomImage` has no setter and the options-taking render entry is private (`:1172`). Workaround: rewrite the PI. |
| L13 | `ColorTable` `.lut` files use a different on-disk layout | fo-dicom `Imaging/ColorTable.cs:72-119` requires exactly 768 bytes, stored as planar R then G then B. radicom `render/color_table.rs:55-94` reads/writes interleaved R,G,B and accepts any length ≥ 768. Existing fo-dicom/classic `.lut` files load with permuted colours; radicom files are rejected by fo-dicom's exact-length check. |
| L14 | 3D geometry utility surface is partial | fo-dicom `Imaging/Mathematics/Geometry3.cs` (Line3/Segment3/Plane3/Slice3/Orientation3, rotate/reflect/distance/nearest-axis) and `Imaging/Mathematics/GeometryHelper.cs:24-37`. radicom `render/math.rs:28-116` has only Point3 and Vector3 length/normalize/dot/cross. No internal render/MPR behaviour was shown to depend on the missing helpers. |
| L15 | XA biplane (retired) still applies the modality LUT | fo-dicom `Imaging/GrayscaleRenderOptions.cs:167-174` suppresses ModalityLUT for the retired biplane SOP class too. radicom `render/mod.rs:635-648` checks only XA and XRF. |
| L16 | `from_bit_range` sets the window in stored space | fo-dicom `Imaging/GrayscaleRenderOptions.cs:446-472` converts the bit range through rescale/LUT output first. radicom `render/mod.rs:445-461` sets it over stored values, then rescales before windowing (`:533-546`). Identity rescale unaffected; no internal path calls this factory. |

### Serialization / logging
| ID | Gap | Evidence |
|---|---|---|
| L17 | JSON reader is stricter than fo-dicom on comments and array-form InlineBinary | fo-dicom `Serialization/SourceGenerationContext.cs:27` skips comments; `Serialization/JsonDicomConverter.cs:1143-1154` accepts `"InlineBinary": ["<base64>"]`. radicom `radicom-json/src/lib.rs:517-518` uses `serde_json::from_str` (no comments) and `:595-607` accepts only a string, silently yielding an empty element otherwise. |
| L18 | No file-level dump/log; the file-meta group is never shown | fo-dicom `Log/Extensions.cs:18-40` walks `FileMetaInfo` then `Dataset`; the Dump tool does the same. radicom `logging.rs:30-56` and `dump/mod.rs:38` take a `Dataset`; `bin/radicom-dump.rs:63-72` prints only `file.dataset` (+ syntax UID), so group 0002 is invisible in the CLI. |
| L19 | Dump/CLI omit element byte length, UID registry names and per-fragment sizes | fo-dicom `Log/DicomDatasetDumper.cs:56-104` prints `# <length>, <keyword>`; `Tools/FO-DICOM.Dump/DumpWalker.cs:53-61,122-131` appends UID names and fragment sizes. radicom `dump/mod.rs:64-71,85-91` prints tag/VR/keyword/value and a fragment count only, although `Uid::name()` (`model/uid.rs:166`) is available. |
| L20 | Parser trace omits element size and dictionary name | fo-dicom `Log/DicomParserLogger.cs:29-38`. radicom `io/trace.rs:27-44` logs position, depth, group/element and VR only. |
| L21 | Per-attribute `keyword`/`name` JSON annotation absent | fo-dicom `Serialization/JsonDicomConverter.cs:82-89,367-380` (`WriteKeyword`, `WriteName`, explicitly non-standard). radicom `JsonOptions` (`radicom-json/src/lib.rs:82-98`) has no such option. |

## Checked and equivalent (not gaps)

- **Identifiers/container:** Tag equality/ordering by group/full element/creator (matching fo-dicom
  `CompareTo`, docs §1.2), MaskedTag, all 34 VRs and their encoding metadata, VM, UID registry and
  generation (2.25 UUID), transfer syntax table + registration, dictionary precedence, runtime XML
  loading, private sub-dictionaries, one-shot default replacement.
- **Dataset:** get/get_at/try_get/get_or/multi-value (`Vec<T>`), sequences, functional groups,
  walker, copy/copy-tags/copy-masked, private-tag block allocation, content equality, deep clone.
- **Charset/validation rules:** the full PS3.15 charset term table, ISO 2022 designators, encoder
  probing and registration; per-VR validators and VM checks (the gap is *when* they run, M3).
- **Buffers/IO:** the complete buffer family (memory/empty/range/composite/endian/even/lazy/file
  window/stream window/temp spill/bulk URI) with fallible reads; byte sources/targets; file and
  directory references; temporary files (including Windows retry-delete).
- **Reader/writer:** 8 observer events, T1–T10 tolerance, CP-246, CP-1066, deflate, fragment
  padding, explicit/undefined lengths, byte-order swap, meta-group refresh.
- **Network wire:** all 7 PDUs, presentation-context result layout, role selection, SOP-class and
  common extended-negotiation codecs, user identity, max PDU, all C- and N-service codecs, C-CANCEL
  (better than fo-dicom, which declares it without sending), status classification/masks/registry,
  cancellation modes, association reuse/renegotiation, TLS+mTLS.
- **Imaging:** pixel facade and encapsulated frame assembly, overlays incl. embedded high bits,
  all fo-dicom photometric interpretations, grayscale/RGB/palette pipelines, VOI/modality/output/
  inverting LUTs, `GrayscaleRenderOptions` factories, FrameGeometry/localizers, SpatialTransform,
  single-frame-series MPR, RawImage + PNG/BMP, icons, DICOMDIR read/write/dedup/scanner.
  fo-dicom's `ILUT`/`IImage`/`IImageManager` interface families are deliberate shape divergences
  (D7); the capabilities exist concretely.
- **Serialization/SR:** JSON carriers, PN groups, number modes, group-length stripping, deferred
  BulkDataURI; XML export shape (export-only on both sides); SR is a superset (SCOORD3D, 15 value
  types, by-reference, validation); anonymizer table is byte-identical to fo-dicom's DefaultProfile
  (the gap is option selection, H5); UID replacement consistency.

## Documented divergences (not counted as gaps)

- Synchronous core (no `OpenAsync`/`SaveAsync`; network layer is async) — architecture §0.
- No built-in codecs (D10); `radicom-codecs` supplies them.
- One `RawImage` + optional backend instead of four .NET image backends (D7).
- No Newtonsoft JSON backend (D11); tracing-convention metrics instead of `opentelemetry` (D12).
- No `CacheType` three-tier cache (documented non-goal in `render/mod.rs`).
- Platform-only items: macOS MAC-address UID, external DICOM peer interop, print/media hardware.

## Documentation corrections (applied)

- Removed the stale `io/pool.rs` / MEM-01 claim from the alignment table: no pool or lease
  implementation exists; MEM-01 remains backlog only in [02-development-guide.md](02-development-guide.md).
- The old module verdict (whose "Not Done" table listed only codecs/platform/peer items) was
  superseded by this audit; the status table above is the current record.
- Historical review ledgers from 2026-10-29/30 were consolidated away; use `git log -- docs/` for
  their content.
- Reproducing the reference: `git clone https://github.com/fo-dicom/fo-dicom.git` and check out
  `d3e6b48426866fa7bf5a52414acca4898f73b6da`; the codec trees are not needed.

## Remaining work

- **L5 (partial):** PFX/PKCS#12 import and OS revocation need a dependency decision; the validation
  callback is implemented.
- **Documented ceilings** (see the status table): C-GET/C-MOVE/N-ACTION serialization on the SCP,
  generic `Read+Seek` large-value windowing, and the one-time dataset materialization on receive.
- **Out of scope:** concrete image codecs (interfaces and transcoding are in scope), the macOS MAC
  address, and external-peer regression testing.

Every other finding above is fixed with a regression test; the workspace suite, clippy and rustdoc
are green.
