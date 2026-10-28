# radicom Feature List

> Version: v0.1 (2026-10-27)
> Basis: a full analysis of the fo-dicom (Fellow Oak DICOM) source code (v5.2.x / 6.0.0-alpha, DICOM standard 2025d, bundled dictionaries 2026b).
> Purpose: defines the feature surface radicom must implement, serving as input for the architecture design and the development plan. Every feature carries an ID (referenced by the development plan), a priority and a scope marker.

## 0. Scope and Priorities

**In scope (radicom)**: DICOM data model, file reading/writing, network communication (DIMSE), serialization, media files, structured reports, printing data model, anonymization, pixel data access facade, codec **interfaces** and the transcoder, rendering pipeline.
**Out of scope (radicom-codecs, a separate project)**: the concrete **implementations** of JPEG / JPEG-LS / JPEG 2000 / HTJ2K / RLE codecs. radicom only defines the interfaces, parameter system, registration mechanism and transcode flow; radicom-codecs mirrors fo-dicom.Codecs (the native codec integration package) and injects its implementations through the registration point exposed by radicom.

**Explicitly handled fo-dicom components** (present in fo-dicom as .NET platform packages/tools; the *capabilities* are all planned in radicom with Rust-native shapes — only the .NET-specific packaging is not carried over; established by the coverage audit against the full fo-dicom file inventory, 2026-10-27):

| fo-dicom component | radicom equivalent |
|---|---|
| `FO-DICOM.AspNetCore` (IHostedService wrapper running a DicomServer; general-purpose C-STORE SCP with InstanceReceived event; port/AE config binding) | **HOST-01** (P2): config-driven server hosting helper on top of `radicom-network`, framework-agnostic |
| `FO-DICOM.Instrumentation` (System.Diagnostics.Metrics / OpenTelemetry wiring of the metrics collector) | **OBS-02** (P2): feature-gated OpenTelemetry wiring of the OBS-01 metrics trait |
| `Serialization/FO-DICOM.Json` (second JSON backend on Newtonsoft.Json) | JSON capability covered by JSON-01 (serde backend). A second JSON backend is redundant in Rust — divergence D11 |
| `Tools/FO-DICOM.Dump` (WPF GUI dump application) | **INF-07** (P2): `radicom-dump` CLI tool (CLI instead of GUI) + INF-04 library dump |
| Platform image backends WinForms / WPF / SkiaSharp (+ the NetStandard packaging variant) | Rendering capability covered by REND-07: single optional `image` backend + dependency-free RawImage — divergence D7 (four .NET backends collapse to one cross-platform backend) |
| Desktop GDI+ print rendering extensions (`FilmBoxExtensions.Print` / `ImageBoxExtensions.Print`) | **PRINT-02** (P2): cross-platform FilmBox layout rendering via the render pipeline |
| Managed codecs inside Core (DICOM RLE encode/decode + JPEG Lossless Process 14/14 SV1 decode-only) | radicom ships **zero built-in codecs**: every implementation lives in radicom-codecs — divergence D10 |
| Legacy client plumbing (`DicomClientConnection`, `DicomClientState`/`StateChanged` compatibility shims, `IDicomService` sync interface) | Replaced by the async-native expert layer (CLI-02); fo-dicom keeps them only for backward compatibility |

**Priority definitions**

| Level | Meaning |
|---|---|
| P0 | Core requirement, part of the MVP; the library is unusable without it |
| P1 | Important feature; should be done before the official release |
| P2 | Enhancement; can be deferred to later iterations |

---

## 1. Core Identifier System (Tag / VR / UID / Transfer Syntax / Dictionary)

| ID | Feature | Description (mirroring fo-dicom behavior) | Priority |
|---|---|---|---|
| TAG-01 | Tag structure | `(group, element)` u16 pair; `is_private` (odd group); conversion to/from u32; Display/Parse (`(GGGG,EEEE)`, `(GGGG,EEEE:Creator)` formats) | P0 |
| TAG-02 | Private tag semantics | Equality/ordering/hashing by the three-part rule "group + low 8 bits of element + PrivateCreator string"; private block normalization (dictionary-style tag with `element ≤ 0xFF` → actual tag `(block<<8)\|elem`) | P0 |
| TAG-03 | Tag constant table | ~5347 `Tag` constants generated from the fo-dicom `DICOM Dictionary.xml` (including command group 0000, meta group 0002, retired tags), via code generation | P0 |
| TAG-04 | Tag interning index | Global tag registry (u32 → Tag); the parser and AT elements reuse known tag instances to reduce allocations | P1 |
| TAG-05 | Masked tags | `MaskedTag` (card + mask, `xx` wildcard) for repeating groups (60xx overlays), private blocks and masked dictionary entries; Parse/Display/IsMatch | P1 |
| VR-01 | VR enum and metadata | The 34 standard VRs; encoding attributes per VR: length-field width (16/32 bit), padding byte, max single-value length, unit size, is-string, is-character-set-sensitive, is-multi-value | P0 |
| VR-02 | VR parsing | Both string and two-letter-byte Parse/TryParse (byte variant dispatches on the first byte for hot-path speed) | P0 |
| UID-01 | UID typing | 12 UID type classifications (TransferSyntax/SOPClass/SOPInstance/ContextGroupName...) plus heuristic storage-category derivation (IsImageStorage etc.) | P0 |
| UID-02 | UID registry | ~1928 built-in UID constants (generated; including 63 transfer syntaxes and ~60 vendor-private SOP classes); Parse returns interned instances on registry hits; Register/Query/Enumerate | P0 |
| UID-03 | UID generation | ① `2.25.<decimal uuid>` UUID derivation (PS3.5 B.2); ② sequence append `Append(base, seq)`; ③ consistent re-mapping generator (the same source UID always maps to the same new UID); ④ recursive `RegenerateAll` (including nested SQ) | P0 |
| UID-04 | Meta SOP class mapping | Print management Meta SOP ↔ sub SOP class lookup, both directions | P2 |
| UID-05 | Network identifier utility | Derive a DICOM UID from the host MAC address (machine identity for AE/network use) — fo-dicom `NetworkManager.TryGetNetworkIdentifier` | P2 |
| TS-01 | Transfer syntax registry | ~57 built-in transfer syntax constants (Implicit/Explicit VR LE, Explicit BE, Deflated, the JPEG family, JPEG-LS, J2K, HTJ2K, MPEG2/4/HEVC video, RLE, etc.), with attributes explicit/encapsulated/lossy/lossy-method/deflate/endianness/swap-pixel-data/retired | P0 |
| TS-02 | Transfer syntax lookup | Parse (unregistered UIDs typed as TransferSyntax are constructed on the fly with default assumptions), Lookup (errors when unregistered), Query (returns None), Register/Unregister (**the registration extension point for radicom-codecs**) | P0 |
| TS-03 | Virtual syntaxes | ImplicitVRBigEndian pseudo syntax (for mis-encoded big-endian implicit files), GE private big-endian (LE + SwapPixelData) | P1 |
| DICT-01 | Dictionary structure | Entry: tag/maskedTag/name/keyword/VR list/VM/retired; built-in public dictionary (5347 tags) + private dictionary (~4678 tags / 235 creators), lazily loaded from gzip-embedded resources | P0 |
| DICT-02 | Dictionary lookup | By tag (private sub-dictionary first → private-creator fallback entry → exact table → masked stack → Unknown fallback); keyword reverse lookup; private creator interning; unknown-tag fallback entry | P0 |
| DICT-03 | External dictionaries | Runtime loading of custom/private dictionaries from XML (gz supported); one-shot replacement injection of the default dictionary | P1 |
| VM-01 | VM representation | Minimum/Maximum/Multiplicity; Parse("1"/"1-n"/"2-2n") and Display round-trip | P1 |

## 2. Dataset and Elements

| ID | Feature | Description | Priority |
|---|---|---|---|
| DS-01 | Dataset container | Ordered storage (sorted by tag: BTreeMap or sorted Vec) so iteration equals file order; deep-copying constructor for nested SQ; setting the internal transfer syntax cascades into nested items | P0 |
| DS-02 | Read API (three tiers) | `get_*` (Err on failure) / `try_get_*` (Option) / `get_or` (default value): single value, value array, string view, raw item, SQ, value count, zero-copy raw bytes | P0 |
| DS-03 | Write API | `insert` / `insert_or_update` (item, typed value, explicit VR, multi-value `\`-separated parsing, empty value → zero-length element); VR-dispatch matrix by Rust type; `remove(tag/predicate)` / `clear` | P0 |
| DS-04 | Private tag operations | `get_private_tag(create)`: scans private-creator blocks, automatically writes the PrivateCreator element when absent, handles group exhaustion; private tag normalization before every read/write | P0 |
| DS-05 | Copy and clone | `clone` (deep copy, skipping validation), `copy_to` (all / selected tags / masked filter) | P0 |
| DS-06 | Functional groups flattening | Enhanced multi-frame: SharedFunctionalGroups + PerFrameFunctionalGroups[frame] → temporary dataset (handling ReferencedImageSequence and other special cases); input to the rendering and geometry modules | P1 |
| DS-07 | Content equality | Element-by-element deep comparison (recursive SQ, buffer content comparison) as the `PartialEq` semantics | P1 |
| DS-08 | Date-time composition | DA + TM → date-time; reads TimezoneOffsetFromUTC to build a timezone-aware value; Try variant with fault tolerance | P1 |
| DS-09 | Walker | Deep traversal callback trait (element / SQ enter-exit / fragment enter-exit), shared by serializers, dumps and JSON | P0 |
| DS-10 | Group/masked enumeration | `enumerate_group(g)` (excluding group length), `enumerate_masked(mask)` | P2 |
| ELEM-01 | Element type system | Concrete element types by VR: single-string (LT/ST/UT/UR), multi-string `\`-separated (AE/AS/CS/LO/SH/UC), PN (component parsing: family/given/middle/prefix/suffix, `^` grouping `=` language groups), numeric strings (DS/IS: lenient parsing, lazy caching), date-time (DA 6 formats / DT 26 formats / TM 36 formats, range parsing), fixed-width binary (FD/FL/SL/SS/SVL/UL/US/UVL), AT (tag array), the Other family (OB/OW/OL/OD/OF/OVL), UN, UI (lazy parse → UID/TransferSyntax) | P0 |
| ELEM-02 | Value conversion | Conversion matrix element value → Rust types (cross numeric types, Option, enums, f64 vs decimal strings, date-time types); DS comma decimal tolerance, precision demotion beyond 16 chars | P0 |
| ELEM-03 | Fragment sequences | Encapsulated pixel data structure: offset table + fragment list (first fragment is the offset table by convention); OB/OW variants | P0 |
| ELEM-04 | SQ elements | Sequence element (item list container), recursive validation | P0 |
| ELEM-05 | Lazy evaluation | String elements keep only the string when set, encoding + padding deferred to serialization; DS/IS/date lazy parse caching | P1 |
| MATCH-01 | Match rule DSL | 14 rules (Exists/IsEmpty/Equals/StartsWith/.../Wildcard/Regex/OneOf + And/Or/Negate composition) for query filtering | P2 |
| XFORM-01 | Transform rule DSL | 16 rules (Remove/SetValue/MapValue/CopyValue/Regex/Prefix/Append/Trim/Pad/Truncate/ToUpper/ToLower/GenerateUid...), supports recording original values into the Modified Attributes Sequence | P2 |

## 3. Character Sets and Validation

| ID | Feature | Description | Priority |
|---|---|---|---|
| ENC-01 | Character set mapping | ~34 DICOM character set ↔ encoding mappings (ISO_IR series, GBK, GB18030, UTF-8, Shift JIS, KS X 1001, etc.); spelling tolerance (`ISO IR`/`ISO-IR`); unknown charset falls back to ASCII with a warning | P0 |
| ENC-02 | Multi-charset decoding | Scans ESC (0x1B) to split segments and switch encodings per ISO 2022 escape sequences; text VRs reset at CR/LF/TAB/FF, PN resets at `^`/`=` | P0 |
| ENC-03 | Multi-charset encoding | Strict-encoder probing → per-segment trial of subsequent encodings → inject ISO 2022 escape prefixes → replacement-character fallback when all fail | P1 |
| ENC-04 | Custom charset registration | Users register private character sets / remap encodings; encoding → charset reverse lookup (used when writing SpecificCharacterSet) | P1 |
| VAL-01 | VR validators | String validators per VR (length limit, legal characters, format regex, numeric ranges): AE/AS/CS/DA/DS/DT/IS/LO/LT/PN/SH/ST/TM/UI + timezone offset validation; DT range/UTC-suffix rules are the most complex | P0 |
| VAL-02 | VM validation | Checked against dictionary VM Min/Max (private tags, Other family, UC/UN and empty elements exempt) | P1 |
| VAL-03 | Validation switches | Global switch × per-dataset switch × scope guard (parsing skips validation throughout, same as fo-dicom) | P0 |

## 4. File and IO

| ID | Feature | Description | Priority |
|---|---|---|---|
| FILE-01 | DicomFile open | Open from file path / stream / byte array; fallback encoding; stop predicate (stop after a given tag, `is_partial`); error wrapping (DicomFileException carrying file context) | P0 |
| FILE-02 | Large-object read policy | Four modes: Default / ReadLargeOnDemand (large values stay as file/stream windows, lazily loaded) / SkipLargeTags / ReadAll; largeObjectSize threshold (default 64 KB); automatic buffered wrapping for non-seekable streams | P1 |
| FILE-03 | Format detection | 132-byte preamble+DICM / no preamble / no meta info / ACR-NEMA 1/2; big-endian detection; explicit/implicit VR detection (dictionary VR comparison); RecognitionCode and TransferSyntaxUID handling | P0 |
| FILE-04 | File meta information | Dataset subtype restricted to group 0002; strongly typed fields (MediaStorageSOPClass/InstanceUID, TransferSyntax, ImplementationClassUID/VersionName...); pre-save processing (refresh Implementation info, rebuild meta from dataset) | P0 |
| FILE-05 | Save | Writes preamble + meta (always Explicit VR LE + group length recalculation) + dataset (per internal syntax); ACR-NEMA rejected; write options | P0 |
| FILE-06 | Header validation | `has_valid_header` (skip 128 bytes, check DICM); Clone | P1 |
| RD-01 | Parser | Recursive-descent parsing (tag → VR → length → value); result enum (Processing/Success/Error/Stopped/Suspended); SQ depth cap (256) against stack overflow | P0 |
| RD-02 | Parse observers | 8-callback trait (element / SQ enter-exit / SQ item enter-exit / fragment enter-exit); parsing decoupled from building so DICOMDIR/SR/dump/JSON all reuse the parser | P0 |
| RD-03 | Malformed-data tolerance | Private SQ explicit/implicit mismatch detection (peek VR bytes inside the item); UN misjudgment fallback; explicit VR parse failure falls back to implicit (0x2020 → dictionary VR); orphan Item treated as undefined-length SQ; missing fragment terminator tolerance; k-pacs compatibility | P0 |
| RD-04 | Deflate support | Deflated transfer syntax: decompress-then-parse / compress on write | P1 |
| RD-05 | Private creator parsing | Dynamically maintains the (group, block) → creator mapping during parsing; private tags bind their creator immediately | P0 |
| WR-01 | Writer | Walker-based writing (reusing DS-09); explicit/implicit VR headers; endianness handling (unit-size byte-swap wrapping); undefined/explicit-length SQ (private SQ always explicit length); long VR above 0xFFFE demoted to UN (CP-1066); fragment + offset table writing | P0 |
| WR-02 | Length calculator | Exact encoded-size computation per transfer syntax (reused by explicit lengths and DICOMDIR offsets) | P0 |
| WR-03 | Group length handling | Recalculate/remove group length elements (recursive SQ); write options (keep group lengths / explicit-length SQ/items) | P0 |
| BUF-01 | Byte buffer trait | `is_memory/size/data/get_byte_range/copy_to` abstraction; all large values are buffer references, never immediate copies | P0 |
| BUF-02 | Buffer implementations | Memory / Empty (singleton) / Range view / Composite chain / lazy file window / stream window (serialized access) / temp-file spill / Endian+Swap wrappers / even-length padding / lazy evaluation / BulkDataUri reference (for JSON) | P0 (file/stream may be P1) |
| IO-01 | File/directory references | FileReference trait (open/create/move/delete/range read) + injectable factory; temporary files (configurable directory + deferred deleter); directory enumeration | P1 |
| IO-02 | Byte source/target | ByteSource (require/skip/goto/multi-width reads/get_buffer with large-object logic) and ByteTarget (multi-width writes / Writer adaptation), with File and Stream implementations | P0 |
| IO-03 | Endianness utilities | Endian enum, byte swapping, ByteConverter (buffer ↔ typed arrays) | P0 |
| MEM-01 | Memory pool | Large-buffer lease abstraction (pooled allocator); parser scratch buffers and chunked copies go through the pool uniformly | P2 |

## 5. Pixel Data and Codec Interfaces (interfaces in radicom, implementations in radicom-codecs)

| ID | Feature | Description | Priority |
|---|---|---|---|
| PIX-01 | PixelData facade | Dataset-bound pixel data read/write facade: Rows/Columns/NumberOfFrames/BitsAllocated/Stored/HighBit/SamplesPerPixel/PixelRepresentation/PlanarConfiguration/PhotometricInterpretation attribute mapping; OB/OW/Encapsulated implementations; CreateNew/CreateFromDataset factories | P0 |
| PIX-02 | Frame access | `get_frame(i)`: uncompressed = zero-copy range slice; encapsulated = single-frame single-fragment / fragment count == frame count / offset-table aggregation; unified endianness handling (including GE big-endian swap); `add_frame` (even-length padding, offset-table maintenance and overflow clearing) | P0 |
| PIX-03 | Frame size computation | Uncompressed frame byte size with special cases: 1-bit row packing, YBR 422/420 odd widths, uncompressed YBR_FULL_422 subsampling | P0 |
| PIX-04 | Palette LUT extraction | Red/Green/Blue Palette Color LUT Descriptor + Data → color table (size 0 → 65536, 16-bit allocated entries take the high byte (`>>8`), etc.) | P1 |
| PIX-05 | Overlay data access | 60xx-group overlay discovery and validation; standalone OverlayData reading; embedded high-bit overlay extraction from pixel data (uncompressed only); bit expansion; reverse overlay building from an image | P1 |
| CODEC-01 | Codec trait | `name/transfer_syntax/default_params/encode(src_pd, dst_pd, params)/decode(...)`; **touches only the PixelData facade (frame buffers), never other dataset elements** — the narrow interface boundary for radicom-codecs | P0 |
| CODEC-02 | Codec parameter system | Parameter type family: JPEG (quality/smoothing/colorspace conversion/sample factor/predictor/point transform), JPEG 2000 (irreversible/rate/rate levels/MCT/update PI/signed handling), HT-J2K (decomposition levels/color transform/progression order/TLM), RLE (no params), JPEG-LS (near-lossless/interleave/color transform/preset, defined on the codecs side) | P0 |
| CODEC-03 | Codec registry | `CodecRegistry` trait: has_codec/can_transcode/get_codec; **the default registry is empty — the core ships zero codec implementations** (divergence D10); **radicom-codecs provides its own Registry implementation with all codecs incl. RLE and injects it** (mirroring the TranscoderManager injection point) | P0 |
| ~~CODEC-04~~ | ~~Built-in RLE codec~~ | **Dropped (2026-10-27): no built-in codecs in radicom.** The DICOM RLE codec (and every other codec) is implemented in radicom-codecs; the core defines only the interface, parameter system and registration mechanism. See divergence D10 | – |
| CODEC-05 | JPEG bit-depth probing | Scans JPEG SOF markers to infer the actual bit depth (corrects BitsAllocated when decoding). A codec-support **utility** (not a codec) consumed by radicom-codecs during decode; kept in core as part of the shared boundary surface | P2 |
| TR-01 | Transcoder | `Transcoder(in_syntax, out_syntax, params)`: transcode(file/dataset); uncompressed↔uncompressed frame-wise direct copy; compressed↔compressed decode to Explicit VR LE then encode (direct-transcode hook reserved); lossy output auto-adds the LossyImageCompression/Method/Ratio tags; embedded overlay extraction; group length recalculation; meta TransferSyntax update | P0 |
| TR-02 | Single-frame decoding | `decode_frame(dataset, frame)` (clone-isolated, does not mutate the source), `decode_pixel_data` | P0 |
| TR-03 | Convenience API | `DicomFile/DicomDataset::clone_with_syntax(syntax, params)` | P1 |

## 6. Network Communication (DIMSE)

| ID | Feature | Description | Priority |
|---|---|---|---|
| NET-01 | TCP/TLS abstraction | Async (tokio) connection abstraction: client/server, NoDelay, send/receive buffers, connect and read/write timeouts; TLS initiator/acceptor traits (certificate sources, protocol ≥TLS1.2, mutual auth, handshake timeout); IPv4/IPv6 | P0 |
| NET-02 | PDU codec | 7 PDU types (A-ASSOCIATE-RQ/AC/RJ, P-DATA-TF, A-RELEASE-RQ/RP, A-ABORT) read/write; big endian, 16-bit length back-patching, bounds checking, AE trimming; PDV (command/last bits, even length); unknown user items skipped | P0 |
| NET-03 | Association model | Association (AE titles, max PDU, async ops, remote implementation info); PresentationContext (odd ID allocation, multiple candidate transfer syntaxes, result/accepted syntax/SCU-SCP role selection) | P0 |
| NET-04 | Extended negotiation | SOP Class Extended Negotiation (byte-field model + Storage/Find/Get/Move application-info subtypes); Common Extended Negotiation; User Identity (5 types + server response + validation) | P1 |
| NET-05 | Protocol engine | Single-connection state machine (shared by SCU/SCP): receive loop, PDU queue backpressure, DIMSE message queue/pending table/async window counting; PDV reassembly (commands always Implicit VR LE, data per negotiated syntax); message factory for 22 command fields | P0 |
| NET-06 | Streaming send | Large datasets written and chunked into PDVs on the fly (command/data as separate P-DATA-TF, maxPDU/maxPDV/even padding); P-DATA circuit breaker (no further P-DATA after timeout with unsent PDUs); on-the-wire transcoding + unsupported-syntax fallback (strip PixelData or send as-is, configurable) | P0 |
| NET-07 | Timeouts and watchdog | Request timeout by three timestamps (last pending / last PDU / enqueue); timeout events, pending cleanup | P1 |
| NET-08 | Connection lifecycle | Graceful/forced close (queue-drain check); connection-closed callbacks; error propagation chain (dedicated errors: Reject/Abort/RequestTimedOut/RequestTimedOut/PrematureClosed) | P0 |
| SRV-01 | Server | Generic `DicomServer<P: DicomServiceProvider>`: port listening, per-connection tasks, max-clients throttling, same-address registry, Stop/Dispose lifecycle; connected-client statistics | P0 |
| SRV-02 | Built-in C-ECHO SCP | Verification service sample implementation | P0 |
| CLI-01 | Client (high level) | `DicomClient`: request queue, send loop (per-association request cap, automatic new associations), parallel sending (negotiated-window driven), linger reuse, association-request timeout retry (consecutive-N cap), cancellation modes (graceful release vs abort), events (Accepted/Rejected/Released/TimedOut) | P0 |
| CLI-02 | Client (expert level) | Manual connection/association management: one association per connection, event streams (channel-based), per-request response streams (`Stream<Item=Response>`), Release/Abort | P1 |
| HOST-01 | Server hosting helper | Config-driven server lifecycle helper (port/AE title/TL options from config, start/stop/restart, listener health status, connected-client counts) so applications embed a DICOM server without writing the plumbing — the Rust equivalent of fo-dicom's AspNetCore hosting package capability | P2 |
| SRV-03 | C-STORE service | Request (File/additional transfer syntaxes/implicit-VR exemption options) / response; SCP receive spills to a temp file by default (streaming, memory-safe, customizable sink); parse failures answered with ProcessingFailure without dropping the connection | P0 |
| SRV-04 | C-FIND service | Level (Patient/Study/Series/Image + MWL) auto SOP-class selection; five convenience query constructors (Patient/Study/Series/Image/Worklist); SCP streams responses one by one; sub-operation counters | P0 |
| SRV-05 | C-GET service | StudyRoot GET three-level constructors; reverse C-STORE handler (client must register one); sub-operation counters | P1 |
| SRV-06 | C-MOVE service | DestinationAE; three-level constructors; sub-operation counters; Move Originator information written | P1 |
| SRV-07 | N-services | N-EventReport/N-Get/N-Set/N-Action/N-Create/N-Delete messages and provider traits (storage commitment etc.) | P2 |
| SRV-08 | C-CANCEL | Actively cancel an in-progress C-FIND/C-MOVE (fo-dicom only defines it without implementing; radicom fills the gap) | P2 |
| DIMSE-01 | Message model | DicomMessage (command set + data set); command-field enum (bit15 = response); priority; message ID generation (per-association numbering, better than fo-dicom's process-global counter); status code system (~60 presets + mask wildcard matching + Lookup + extensibility + ErrorComment) | P0 |
| OBS-01 | Observability | tracing structured logging across the network layer (classifiable by remote AE); optional metrics collection trait (byte counters/connection events); parse-trace logging | P1 |
| OBS-02 | OpenTelemetry integration | Optional, feature-gated wiring of the metrics trait to OpenTelemetry (`opentelemetry` crates): byte sent/received counters, connection count/gauge, activity source — the Rust equivalent of fo-dicom's Instrumentation package metric names | P2 |

## 7. Serialization / Media / SR / Printing

| ID | Feature | Description | Priority |
|---|---|---|---|
| JSON-01 | DICOM JSON (PS3.18) | Tag keys + vr + Value/InlineBinary/BulkDataURI; PN three-group structure; numeric modes (AsNumber/AsString/PreferablyAsNumber, DS trailing-zero preservation); group length stripped; private creator rebuilding; fragments rejected with a transcode hint; reading tolerates unordered/unknown properties | P1 |
| JSON-02 | BulkData references | Bidirectional BulkDataUri buffer support (lazy load on read, reference on write) | P2 |
| XML-01 | DICOM XML export | One-way NativeDicomModel export (tag/vr/keyword/Value/InlineBinary) | P2 |
| MEDIA-01 | DICOMDIR read/write | DicomDirectory: read (record tree stitched by offsets + nearest-offset tolerance); write (two-pass offset computation, root first/last offsets, exact item-header lengths); AddFile four-level (Patient→Study→Series→Instance) dedup-chain reuse; IMAGE/SR/PRESENTATION record types | P1 |
| MEDIA-02 | Thumbnail interface | `IconGenerator` trait (hook for DICOMDIR IconImageSequence generation) + built-in 8-bit grayscale ≤128×128 builder | P2 |
| MEDIA-03 | File scanner | Asynchronous directory scanning (progress/found events); input for batch DICOMDIR building | P2 |
| SR-01 | SR document tree | ContentItem node tree (Code/14 ValueTypes/7 Relationships/Continuity); Children traversal and add/remove; type-checked constructors; SR open/save (explicit lengths forced) | P1 |
| SR-02 | SR data types | CodeItem (short/long/URN code auto-dispatch + equality semantics), MeasuredValue, ReferencedSOP | P1 |
| PRINT-01 | Printing data model | Full property encapsulation of FilmSession/FilmBox/ImageBox/PresentationLut; ImageDisplayFormat parsing (STANDARD/ROW/COL/SLIDE/CUSTOM); FilmBox generates the ImageBox sequence per format (grayscale/color SOP selection); layout rectangle computation; Load/Save persistence | P2 |
| PRINT-02 | Print layout rendering | Render a FilmBox layout to an image (print preview): per-ImageBox rectangles from ImageDisplayFormat (STANDARD `\C,R` / ROW / COL with margins), EmptyImageDensity background fill, optional Trim border, each ImageSequence rendered via the render pipeline and scaled into its cell — cross-platform equivalent of fo-dicom's GDI+ print rendering, built on REND-07 backends | P2 |
| ANON-01 | Anonymization | Full PS3.15 BasicProfile table (generated, embedded); 11 options × 6 actions (D/Z/X/K/C/U) matrix; regex tag matching (private/overlay/curve wildcard rows); SQ recursion; consistent UID replacement; type-aware blanking; PatientName/ID replacement values; file-level meta sync; in-place and cloning entry points | P1 |

## 8. Rendering (P2, later iterations)

| ID | Feature | Description | Priority |
|---|---|---|---|
| REND-01 | Rendering entry | DicomImage: PI inference (ACR-NEMA compatibility), window center/width/VOI LUT/invert/palette properties, frame caches (pixels/pipeline/output tiers), Scale, overlay composited rendering | P2 |
| REND-02 | Rendering pipelines | Grayscale pipeline (Modality LUT → VOI LUT → output color table → invert, 16-bit precomputed lookup); RGB pipeline; palette pipeline | P2 |
| REND-03 | LUT family | VOI (LINEAR/LINEAR_EXACT/SIGMOID/Sequence), Modality (Rescale/Sequence), Output, Palette, Invert, Padding, Composite, Precalculated | P2 |
| REND-04 | RenderOptions | GrayscaleRenderOptions factory chain (WindowLevel → FunctionalGroups → Smallest/Largest → MinMax → Histogram); XA/XRF modality-LUT exception; LINEAR→LINEAR_EXACT correction | P2 |
| REND-05 | Pixel render types | IPixelData family (U8/S16/U16/S32/U32/1-bit/RGB24): high/low bit masking, min/max, histogram, bilinear/nearest-neighbor scaling, parallel lookup rendering | P2 |
| REND-06 | Color conversion | YBR_FULL/422/Partial422 → RGB, Planar↔Interleaved, ReverseBits, color tables (Mono1/Mono2) | P2 |
| REND-07 | Image backend abstraction | Image trait + dependency-free RawImage (BGRA bytes) default; optional image-rs backend (PNG/BMP export, feature-gated) | P2 |
| REND-08 | Geometry and localizers | FrameGeometry (image ↔ patient coordinate transforms, corner coordinates); ImageLocalizer (projection/intersection localizer lines) | P2 |
| REND-09 | MPR reconstruction | VolumeData arbitrary-plane cutting / Stack resampling / writing reconstruction results back to DICOM | P2 |
| REND-10 | Icon image rendering | Render `(0000,9113)` IconImageSequence as an image (PI restricted to MONOCHROME1/2/PALETTE COLOR; handles icon pixel data stored native inside an encapsulated parent; frames forced to 1) — fo-dicom `DicomIconImage` | P2 |
| REND-11 | Spatial transform and math utilities | SpatialTransform (scale / 90°-step rotation / flipX / flipY / pan state machine); shared math utilities backing geometry and rendering: Point2/Point3/Vector3/Matrix/Rect/Interval/Histogram/BitList/MovingAverage | P2 |

## 9. Infrastructure

| ID | Feature | Description | Priority |
|---|---|---|---|
| INF-01 | Error hierarchy | Error type hierarchy (thiserror): RadicomError → Data/Validation/File/Reader/Writer/Network(Reject/Abort/Timeout/Closed)/Codec/Imaging...; messages carry tag/VR/content context | P0 |
| INF-02 | Implementation identity | ImplementationClassUID / VersionName constants (configurable), used by network and file meta | P0 |
| INF-03 | Logging | tracing facade + categorized targets (network/codec/encoding/io); dataset dump (hierarchical indentation, binary as hex, truncation control) | P1 |
| INF-04 | Dump tooling | dataset → formatted string/console output (mirroring FO-DICOM.Dump) | P1 |
| INF-07 | `radicom-dump` CLI tool | Command-line dump utility: open a DICOM file and print the dataset (tag/VR/name/value, truncation options), file-format diagnostics (detected format/syntax), pixel-data summary (frames, bits, photometric interpretation) — the Rust equivalent of fo-dicom's Dump tool as a CLI instead of a GUI | P2 |
| INF-05 | Code generation | Generated from fo-dicom dictionary XML: Tag constants (~5347), UID constants (~1928), dictionary tables, embedded anonymization profile CSV; build.rs or xtask | P0 |
| INF-06 | Test data and baselines | Test data organized after fo-dicom Tests (samples per transfer syntax, multi-frame, SR, DICOMDIR); parse/write round-trip baselines | P0 |

---

## Appendix: Feature Count Summary

| Module | Items | P0 count |
|---|---|---|
| Core identifiers (Tag/VR/UID/TS/DICT) | 19 | 14 |
| Dataset and elements | 20 | 13 |
| Character sets and validation | 7 | 4 |
| File and IO | 22 | 14 |
| Pixel data and codec interfaces | 12 | 10 |
| Network communication | 23 | 14 |
| Serialization/Media/SR/Printing | 11 | 0 (mostly P1) |
| Rendering | 11 | 0 (P2) |
| Infrastructure | 7 | 4 |
| **Total** | **132** | **73** |

> History: CODEC-04 (built-in RLE codec) was dropped from the core scope on 2026-10-27 (zero built-in codecs; all implementations including RLE belong to radicom-codecs). The 2026-10-27 parity re-audit added UID-05, OBS-02, HOST-01, PRINT-02 and INF-07 so that every fo-dicom platform capability has a planned Rust equivalent.
