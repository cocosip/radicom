# radicom Overall Architecture Design

> Version: v0.1 (2026-10-27)
> Companion documents: [01-feature-list.md](01-feature-list.md) (feature IDs such as TAG-01 are referenced here), [03-development-plan.md](03-development-plan.md)
> Detailed per-module specifications: [04-data-model.md](04-data-model.md), [05-file-io.md](05-file-io.md), [06-pixel-codec.md](06-pixel-codec.md) (the codec boundary contract), [07-network.md](07-network.md), [08-serialization-media.md](08-serialization-media.md), [09-testing.md](09-testing.md), [10-rendering.md](10-rendering.md)
> Reference model: the split between fo-dicom (Fellow Oak DICOM) FO-DICOM.Core and fo-dicom.Codecs

---

## 1. Design Principles

1. **Feature parity with fo-dicom, interfaces redesigned in Rust idiom**: the semantics (behavior details, tolerance strategies, standard compliance) align with fo-dicom; the API does not copy the C# shape one-to-one (no DI container, no inheritance hierarchies) — traits + generics + ownership express the same capabilities.
2. **Zero-copy first**: large values (pixel data, big OW/OB) are always buffer references, never immediate copies (BUF-01); uncompressed frame reads are slice views (PIX-02); strings/numbers evaluate lazily (ELEM-05).
3. **Parsing decoupled from building**: the parser only produces observer events (RD-02); dataset building, DICOMDIR tree stitching, SR, dumps and JSON all reuse the same parser.
4. **Codecs fully decoupled from the core**: the core library has zero compression-library dependencies; codecs are injected via trait + registry, and radicom-codecs is purely downstream (see §6).
5. **Synchronous core + async network**: the data model and IO expose synchronous APIs (simple, FFI-friendly, easy to test); the network layer is fully async on tokio; async file access is adapted via `spawn_blocking`.
6. **Tolerant reading, strict writing**: the read path is lenient (malformed-data tolerance RD-03, Try APIs, validation skipped); the write path guarantees standard compliance (group lengths, padding, length rules).
7. **Generated assets go through code generation uniformly** (INF-05): dictionaries and constant tables are never maintained in handwritten code.

## 2. Overall Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                       Application layer (user code)                 │
├──────────────────────────────┬──────────────────────────────────────┤
│  radicom-network             │  radicom-json / radicom-xml (opt.)   │
│  DIMSE/association/TLS/      │  DICOM JSON PS3.18 / XML export      │
│  SCU/SCP (tokio async)       │  (serde)                             │
├──────────────────────────────┴──────────────────────────────────────┤
│                          radicom (core crate)                       │
│  ┌───────────┐ ┌───────────┐ ┌────────────┐ ┌────────────────────┐  │
│  │ model     │ │ io        │ │ pixel      │ │ media/sr/print/    │  │
│  │ Dataset/  │ │ Reader/   │ │ PixelData  │ │ anon (data models) │  │
│  │ Element/  │ │ Writer/   │ │ Codec trait│ │ DICOMDIR/SR/       │  │
│  │ Tag/VR/   │ │ Buffer/   │ │ Registry   │ │ FilmSession/       │  │
│  │ UID/TS/   │ │ DicomFile │ │ Transcoder │ │ Anonymizer         │  │
│  │ Dict/Enc/ │ │           │ │ (no codecs │ │                    │  │
│  │ Validate  │ │           │ │  built in) │ │                    │  │
│  └───────────┘ └───────────┘ └────────────┘ └────────────────────┘  │
│  ┌───────────────────────┐ ┌──────────────────────────────────────┐ │
│  │ radicom-dict (crate)  │ │ render (feature "render", P2 later)  │ │
│  │ Generated: Tag consts/│ │ DicomImage/LUT/pipelines/image       │  │
│  │ UID consts/dict tables│ │ backend abstraction                  │ │
│  │ /anonymizer profile   │ │                                      │ │
│  └───────────────────────┘ └──────────────────────────────────────┘ │
├─────────────────────────────────────────────────────────────────────┤
│        radicom-codecs (separate project, outside this repo)         │
│   JPEG/JPEG-LS/JPEG2000/HTJ2K native implementations → implement    │
│   the Codec trait, provide a CodecRegistry implementation and        │
│   inject it into the core library                                    │
└─────────────────────────────────────────────────────────────────────┘
```

Layer dependencies (top-down):

```
radicom-network ──┐
radicom-json ─────┼──▶ radicom ──▶ radicom-dict
(apps/tools) ─────┘
radicom-codecs (external) ──depends only on──▶ radicom (public codec/pixel interfaces)
```

## 3. Workspace Layout

```
radicom/
├── Cargo.toml                 # workspace root
├── crates/
│   ├── radicom/               # core: data model + IO + pixel facade + codec interfaces + media/SR/print/anonymizer
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── model/         # tag.rs vr.rs element.rs dataset.rs sequence.rs uid.rs transfer_syntax.rs
│   │   │   │                  # dictionary.rs vm.rs private_creator.rs masked_tag.rs range.rs
│   │   │   ├── encoding/      # charset.rs (multi-charset encode/decode)
│   │   │   ├── validation/    # validators + switches
│   │   │   ├── io/            # buffer/ reader/ writer/ file.rs byte_source.rs byte_target.rs endian.rs
│   │   │   ├── pixel/         # pixel_data.rs fragment.rs overlay.rs codec/ (trait+params+registry) transcoder.rs
│   │   │   ├── media/         # dicom_directory.rs icon.rs scanner.rs
│   │   │   ├── sr/            # structured_report.rs code_item.rs content_item.rs
│   │   │   ├── printing/      # film_session.rs film_box.rs image_box.rs presentation_lut.rs
│   │   │   ├── anonymize/     # anonymizer.rs profile.rs (generated)
│   │   │   ├── render/        # feature "render": dicom_image.rs lut/ pipeline/ pixel_impl/
│   │   │   └── error.rs
│   │   └── tests/
│   ├── radicom-dict/          # pure generated-artifact crate (build.rs or xtask-generated code)
│   │   ├── build.rs           # generates constants and static tables from dictionaries/*.xml.gz
│   │   ├── dictionaries/      # DICOM Dictionary.xml(.gz), Private Dictionary.xml(.gz) (from fo-dicom, MIT)
│   │   └── src/lib.rs         # pub use of generated tag::*/uid::*/dict static tables
│   ├── radicom-network/       # network layer (depends on radicom)
│   │   └── src/               # pdu/ association/ service/ (protocol engine) server/ client/ dimse/ (messages+services) tls/
│   └── radicom-json/          # DICOM JSON serialization (serde_json) (P1; XML export may live here or behind a feature)
├── xtask/                     # codegen tasks (fo-dicom dictionary XML → radicom-dict sources)
├── docs/                      # this document series
└── tests-data/                # shared test data (mirroring fo-dicom Tests/Data)
```

**Rationale for the crate split**

| Crate | Why separate |
|---|---|
| `radicom-dict` | Generated code is large (~5347 tags + ~1928 UIDs + dictionary tables); independent compile cache; dictionary upgrades (tracking the DICOM standard version) touch only this crate; zero external dependencies |
| `radicom` | Data model and IO are tightly coupled (elements ↔ buffers ↔ readers/writers); merging avoids circular dependencies and cross-crate trait-object overhead; codec **interfaces** live here, implementations do not |
| `radicom-network` | Pulls in the heavy tokio dependency, optional; file-processing tools outside PACS use cases don't need to compile an async runtime |
| `radicom-json` | serde_json dependency optional; PS3.18 semantics evolve independently |
| `radicom-codecs` (external project) | Native-dependency isolation, independent release cadence — mirroring fo-dicom.Codecs |

## 4. Core Type Design (Draft)

> The following are signature drafts for the key traits/structs. Fine-tuning at implementation time is allowed, but the **boundary semantics** (who owns the data, who reports errors, what a codec can see) are fixed.

### 4.1 Identifier System

```rust
// model/tag.rs
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Tag {
    group: u16,
    element: u16,
    private_creator: Option<Arc<PrivateCreator>>, // private tags carry their creator
}
// PartialEq/Ord semantics: group → element (private compares only the low 8 bits) → creator string
impl Tag { pub const PATIENT_ID: Tag = ...; /* the 5347 generated constants live in radicom-dict */ }

pub struct MaskedTag { card: u32, mask: u32 }   // "60xx,3000"
impl MaskedTag { pub fn is_match(&self, tag: Tag) -> bool; pub fn parse(s: &str) -> Result<Self>; }

// model/vr.rs
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Vr { AE, AS, AT, CS, DA, DS, DT, FD, FL, IS, LO, LT, OB, OD, OF, OL, OV,
              OW, PN, SH, SL, SQ, SS, ST, SV, TM, UC, UI, UL, UN, UR, US, UT, UV,
              None /* internal: items/delimiters */, Implicit /* implicit placeholder inside explicit data */ }
impl Vr {
    pub const fn code(self) -> &'static str;
    pub const fn is_string(self) -> bool;
    pub const fn is_character_encoded(self) -> bool; // affected by SpecificCharacterSet
    pub const fn is_16bit_length(self) -> bool;
    pub const fn unit_size(self) -> usize;
    pub const fn padding(self) -> u8;               // 0x00 or 0x20
    pub const fn max_length(self) -> u32;
    pub fn parse(code: &[u8; 2]) -> Result<Self, Error>; // first-byte lookup table
}
```

### 4.2 Elements and Dataset

```rust
// model/element.rs —— an element = tag + VR + value payload (enum instead of a class hierarchy)
pub enum DicomItem {
    Element(DicomElement),
    Sequence(DicomSequence),
    FragmentSequence(DicomFragmentSequence),
}

pub struct DicomElement {
    tag: Tag,
    vr: Vr,
    value: ElementValue,
}
pub enum ElementValue {
    Str { text: String, encodings: CharEncodings },       // lazy: bytes not yet materialized
    Bytes(ByteBuffer),                                     // materialized (from parsing / binary VRs)
    ParsedDates { dates: Box<[DicomDateTime]>, formats }, // DA/DT/TM parse cache
}
impl DicomElement {
    pub fn get<T: FromElement>(&self, index: Option<usize>) -> Result<T>;
    pub fn to_bytes(&self, encodings: &CharEncodings) -> Cow<[u8]>; // encode + pad at write time
}

// model/dataset.rs —— ordered container, Rust-style three-tier reads
pub struct Dataset {
    items: BTreeMap<TagKey, DicomItem>, // TagKey = (u16,u16, creator ordinal); BTreeMap keeps order
    transfer_syntax: TransferSyntax,
    validate: bool,
    fallback_encodings: CharEncodings,
}
impl Dataset {
    pub fn get<T: FromElement>(&self, tag: impl Into<Tag>) -> Result<T>;            // Err = missing or type mismatch
    pub fn try_get<T: FromElement>(&self, tag: impl Into<Tag>) -> Option<T>;
    pub fn get_or<T: FromElement>(&self, tag: impl Into<Tag>, default: T) -> T;
    pub fn get_str(&self, tag) -> Result<Cow<str>>;
    pub fn get_sequence(&self, tag) -> Result<&Sequence>;
    pub fn insert(&mut self, item: impl Into<DicomItem>) -> &mut Self;
    pub fn insert_value<V: IntoElement>(&mut self, tag, value: V) -> &mut Self;
    pub fn remove(&mut self, tag) -> Option<DicomItem>;
    pub fn get_private_tag(&mut self, tag: Tag, create: bool) -> Option<Tag>;
    pub fn functional_groups(&self, frame: u32) -> Result<Dataset>;
    pub fn walk(&self, walker: &mut impl DatasetWalker);
}
```

### 4.3 IO: Buffers, Parsing, Writing

```rust
// io/buffer.rs
pub trait ByteBuffer: Send + Sync {
    fn size(&self) -> u64;
    fn is_memory(&self) -> bool;
    fn data(&self) -> Cow<[u8]>;                        // materialize (may trigger lazy load)
    fn get_byte_range(&self, offset: u64, count: usize, out: &mut [u8]);
    fn copy_to(&self, w: &mut impl std::io::Write) -> io::Result<u64>;
}
pub enum ByteBuffer { /* or concrete types + Arc<dyn ByteBuffer> composition */
    Memory(Bytes), Empty, Range{ inner: Arc<ByteBuffer>, offset, len },
    Composite(Vec<Arc<ByteBuffer>>),
    File { path: PathBuf, position: u64, len: u64 },     // FileByteBuffer
    Stream { /* StreamByteBuffer: internal Mutex serializing seek+read */ },
    Endian { inner: Arc<ByteBuffer>, unit: usize },      // Swap wrapper
    BulkDataUri { uri: String, data: OnceCell<Bytes> },
}
```

```rust
// io/reader.rs —— parsing decoupled from building (RD-02)
pub trait ParseObserver {
    fn on_element(&mut self, ctx: &ParseContext, tag: Tag, vr: Vr, buffer: ByteBuffer) -> Result<()>;
    fn on_begin_sequence(&mut self, ctx: &ParseContext, tag: Tag, len: u32) -> Result<()>;
    fn on_begin_sequence_item(&mut self, ctx: &ParseContext, len: u32) -> Result<()>;
    fn on_end_sequence_item(&mut self, ctx: &ParseContext) -> Result<()>;
    fn on_end_sequence(&mut self, ctx: &ParseContext) -> Result<()>;
    fn on_begin_fragment(&mut self, ctx: &ParseContext, tag: Tag, vr: Vr) -> Result<()>;
    fn on_fragment(&mut self, ctx: &ParseContext, buffer: ByteBuffer) -> Result<()>;
    fn on_end_fragment(&mut self, ctx: &ParseContext) -> Result<()>;
}
pub struct ParseContext { pub position: u64, pub depth: u32 }

pub struct DicomReader { /* options: read_option, large_object_size, max_depth=256 */ }
impl DicomReader {
    pub fn read(&self, source: &mut impl ByteSource, syntax: &TransferSyntax,
                observer: &mut impl ParseObserver, stop: Option<StopFn>) -> ReadResult;
    // ReadResult: Success | Stopped | Suspended | Error (Suspended enables incremental/streaming parsing)
}

// io/file.rs
pub struct DicomFile { pub file_meta: FileMetaTable, pub dataset: Dataset, format: FileFormat, is_partial: bool }
impl DicomFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self>;
    pub fn from_reader(r: impl Read + Seek, opts: ReadOptions) -> Result<Self>;
    pub fn save(&self, path) -> Result<()>; pub fn write(&self, w: impl Write) -> Result<()>;
}
pub enum FileReadOption { Default, ReadLargeOnDemand, SkipLargeTags, ReadAll } // FILE-02
```

### 4.4 Pixel Data and Codec Interfaces (CODEC/TR/PIX)

```rust
// pixel/pixel_data.rs —— the facade: the only pixel interface seen by codecs and rendering
pub struct PixelData<'a> { dataset: &'a mut Dataset, /* kind: OB | OW | Encapsulated */ }
impl<'a> PixelData<'a> {
    pub fn create_new(ds: &mut Dataset) -> Result<PixelData>;       // OB/OW/fragment per syntax and bit depth
    pub fn from_dataset(ds: &'a Dataset) -> Result<PixelData<'a>>;
    pub fn width(&self) -> u32; pub fn height(&self) -> u32;
    pub fn bits_allocated(&self) -> u16; pub fn bits_stored(&self) -> u16;
    pub fn number_of_frames(&self) -> u32;
    pub fn photometric_interpretation(&self) -> Option<PhotometricInterpretation>;
    pub fn get_frame(&self, index: u32) -> Result<ByteBuffer>;      // zero-copy / aggregation / endianness handling
    pub fn add_frame(&mut self, data: impl AsRef<[u8]>) -> Result<()>;
    pub fn transfer_syntax(&self) -> &TransferSyntax;
}

// pixel/codec/mod.rs —— the single interface implemented by radicom-codecs (CODEC-01)
pub trait Codec: Send + Sync {
    fn name(&self) -> &str;
    fn transfer_syntax(&self) -> &TransferSyntax;
    fn default_params(&self) -> CodecParams;
    fn encode(&self, src: &PixelData, dst: &mut PixelData, params: &CodecParams) -> Result<()>;
    fn decode(&self, src: &PixelData, dst: &mut PixelData, params: &CodecParams) -> Result<()>;
}
/// Parameter system: an enum instead of inheritance (CODEC-02)
pub enum CodecParams {
    None,
    Jpeg(JpegParams),           // quality/smoothing/convert_colorspace/sample_factor/predictor/point_transform
    Jpeg2000(J2kParams),        // irreversible/rate/rate_levels/allow_mct/update_pi/signed_as_unsigned
    HtJpeg2000(HtJ2kParams),    // decompositions/color_transform/progression_order/insert_tlm
    JpegLs(JpegLsParams),       // defined on the radicom-codecs side; the enum variant is reserved here (opaque passthrough is also fine)
}

// pixel/codec/registry.rs —— the registry (CODEC-03)
pub trait CodecRegistry: Send + Sync {
    fn has_codec(&self, syntax: &TransferSyntax) -> bool;
    fn can_transcode(&self, from: &TransferSyntax, to: &TransferSyntax) -> bool;
    fn get_codec(&self, syntax: &TransferSyntax) -> Result<Arc<dyn Codec>>;
}
/// Globally replaceable registry (mirroring the ITranscoderManager DI injection point)
pub fn set_codec_registry(r: Arc<dyn CodecRegistry>);
pub fn codec_registry() -> Arc<dyn CodecRegistry>;  // default is empty — zero built-in codecs (D10)

// pixel/transcoder.rs (TR-01)
pub struct Transcoder { from: TransferSyntax, to: TransferSyntax, in_params: CodecParams, out_params: CodecParams }
impl Transcoder {
    pub fn transcode_file(&self, file: &DicomFile) -> Result<DicomFile>;
    pub fn transcode_dataset(&self, ds: &Dataset) -> Result<Dataset>;
    pub fn decode_frame(&self, ds: &Dataset, frame: u32) -> Result<ByteBuffer>;
    pub fn extract_overlays(ds: &mut Dataset) -> Result<()>;   // embedded overlay extraction
}
```

### 4.5 Network Layer (radicom-network)

```rust
// pdu/ —— NET-02: codec for the 7 PDU types + RawPdu reader/writer
pub enum RawPdu { AAssociateRq(..), AAssociateAc(..), AAssociateRj{result, source, reason},
                  PDataTf(Vec<Pdv>), AReleaseRq, AReleaseRp, AAbort{source, reason} }
pub struct Pdv { pub presentation_context_id: u8, pub message_header: MessageHeader, pub data: Bytes }

// association/ —— NET-03/04
pub struct Association { calling_ae, called_ae, presentation_contexts: PresentationContexts,
                         max_pdu_length: u32, max_async_ops: (u16, u16),
                         remote_implementation: (String, String),
                         user_identity: Option<UserIdentity>, extended_negotiations: .. }

// service/ —— NET-05: the single-connection protocol engine (shared by SCU/SCP)
pub struct DicomService { /* connection + PDU queue + pending table + async window */ }

// SCP-side traits (mirroring IDicomXxxProvider)
#[async_trait]
pub trait CEchoProvider: Send + Sync {
    async fn on_c_echo(&self, assoc: &Association, req: &CEchoRequest) -> CEchoResponse;
}
#[async_trait]
pub trait CStoreProvider: Send + Sync {
    async fn on_c_store(&self, assoc: &Association, req: CStoreRequest) -> CStoreResponse;
    async fn on_c_store_error(&self, err: &Error);   // mirrors OnCStoreRequestException
}
#[async_trait]
pub trait CFindProvider: Send + Sync {
    fn on_c_find(&self, assoc: &Association, req: &CFindRequest)
        -> BoxStream<'static, CFindResponse>;         // streaming multi-response
}
// C-GET/C-MOVE/N-services follow the same pattern

pub struct DicomServer<P> { /* port listener + per-connection tasks + throttling + registry */ }
impl<P: ServiceProvider> DicomServer<P> {
    pub async fn bind(addr: SocketAddr, provider: P, options: ServerOptions) -> Result<Self>;
    pub fn connected_clients(&self) -> usize; pub fn is_listening(&self) -> bool;
    pub async fn shutdown(self);
}

// client/ —— CLI-01/02
pub struct DicomClient { host, port, calling_ae, called_ae, tls: Option<TlsConfig>, /* options */ }
impl DicomClient {
    pub async fn send(&self, requests: Vec<Request>) -> Result<SendSummary>; // queue + automatic association management
    pub async fn send_c_echo(&self) -> Result<Status>;
    pub async fn send_c_store(&self, file: &DicomFile) -> Result<Status>;
    pub async fn send_c_find(&self, req: CFindRequest) -> Result<BoxStream<CFindResponse>>;
}
```

## 5. Error Hierarchy (INF-01)

```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("data error: {0}")] Data(#[from] DataError),          // includes Validation(tag, vr, content)
    #[error("file error: {0}")] File(#[from] FileError),
    #[error("parse error: {0}")] Reader(#[from] ReaderError),
    #[error("write error: {0}")] Writer(#[from] WriterError),
    #[error("network error: {0}")] Network(#[from] NetworkError), // includes Reject{result,source,reason}
                                                                 //     / Abort / Timeout / ClosedPrematurely
    #[error("codec error: {0}")] Codec(#[from] CodecError),
    #[error("imaging error: {0}")] Imaging(#[from] ImagingError),
    #[error("io: {0}")] Io(#[from] std::io::Error),
}
```

Rules: error messages must carry context (tag, VR, value fragment, position); `no_std` is not a goal; network errors are subtyped so reconnect policies can discriminate.

## 6. The radicom ↔ radicom-codecs Boundary (Key Design)

Mirroring fo-dicom's model where FO-DICOM.Core defines IDicomCodec/TranscoderManager and fo-dicom.Codecs injects a NativeTranscoderManager:

**The stable API surface radicom promises (the only interfaces radicom-codecs may depend on):**

1. The `Codec` trait and the `CodecParams` enum (§4.4);
2. The `PixelData` facade (frame read `get_frame` / frame write `add_frame` / pixel properties / syntax) — **codecs never see other Dataset elements**;
3. The `CodecRegistry` trait + the global `set_codec_registry()` injection point + `TransferSyntax::register()`;
4. `ByteBuffer` (the carrier of frame data), `Error::Codec`;
5. The `Transcoder` handles: syntax decisions, the frame loop, Lossy* tag bookkeeping, overlay extraction, group length recalculation — codecs do pure per-frame encode/decode only.

**radicom-codecs responsibilities (out of scope here):**

- **Every** codec implementation behind the `Codec` trait: JPEG (baseline/extended/lossless14/SV1), JPEG-LS, JPEG 2000, HTJ2K **and DICOM RLE** (the core ships none of them);
- Its own `CodecRegistry` implementation (registering all implementations by transfer syntax at construction); users inject it via `set_codec_registry(...)` during initialization; without it, transcode/decode of encapsulated syntaxes returns `Error::Codec(NotRegistered)`;
- The concrete field definitions of codec parameters (the core library's `JpegLsParams` variant etc. is passed through opaquely or extended on the codecs side).

**Version coordination**: radicom's codec interface follows semver; interface changes are major; radicom-codecs' CI runs a compatibility matrix against each minor version of radicom.

## 7. Async Model and Runtime

- `radicom` (core): synchronous API. Rationale: parsing/writing are CPU + sequential-IO bound; a synchronous implementation is simpler, reliable, easy to embed via FFI and in WASM; bulk file processing is parallelized by callers with rayon/thread pools.
- `radicom-network`: fully async on tokio. One connection = one protocol-engine task (receive loop + write queue); one oneshot channel per request; C-FIND/C-MOVE responses use `futures::Stream`.
- Convenience wrappers: `radicom-network` provides `spawn_blocking`-wrapped `open_file_async`-style helpers so async users never mix synchronous calls.

## 8. Dependency Choices

| Purpose | Choice | Notes |
|---|---|---|
| Byte buffers | `bytes` | Bytes/BytesMut zero-copy slicing; the basis for Range/Composite buffers |
| Character sets | `encoding_rs` | Covers GBK/GB18030/Shift_JIS/EUC-KR/ISO-8859 family; ISO 2022 escape-sequence handling implemented by us (ENC-02/03) |
| Async runtime | `tokio` (network crate) | net/rustls/time features enabled as needed |
| TLS | `tokio-rustls` + `rustls-pemfile` | Certificate loading; a custom acceptor trait is exposed |
| Serialization | `serde` / `serde_json` (json crate) | DICOM JSON is a hand-written Converter (the structure is special, no derive) |
| Logging | `tracing` | Unified feature-gated facade |
| Errors | `thiserror` | Standard across the library |
| Time | `chrono` (default-features off) | DICOM DA/DT/TM parsing and timezone offsets |
| Lazy caches | `once_cell` | Global registries (tag index / UID registry / dictionary) |
| Regex (anonymizer/matching) | `regex` | anonymize/match features only |
| Compression | `flate2` (rust backend) | Deflated transfer syntax |
| Rendering backend (P2) | `image` (optional feature) | PNG/BMP export; the core still ships a dependency-free RawImage |
| Codegen tooling | `xtask` + `quick-xml` + `flate2` | Dictionary XML → Rust constants/static tables |

The core `radicom` crate keeps mandatory dependencies at the bytes/thiserror/once_cell/chrono/encoding_rs/flate2 level; heavy dependencies (tokio/serde/regex/image) are isolated behind optional crates or features.

## 9. Code Generation (INF-05)

- Inputs: the fo-dicom `Dictionaries/DICOM Dictionary.xml` and `Private Dictionary.xml` (**MS-PL licensed** — fo-dicom and its dictionaries ship under the Microsoft Public License; the redistributed XML and generated tables must retain the copyright/license notice and attribution), the PS3.15 profile CSV embedded in `DicomAnonymizerGenerated.cs`, and the UID registry source.
- Tooling: `xtask gen-dict` (quick-xml parsing → generated Rust sources in `radicom-dict/src/generated/`; **generated artifacts are committed**, so normal builds never require xtask).
- Generated content: `tag.rs` (5347 constants + display names), `uid.rs` (~1928 constants), `dict.rs` (phf/sorted static tables: tag→(name,keyword,vrs,vm,retired), keyword→tag, private sub-dictionaries), `anon_profile.rs` (static CSV rows).
- Update flow: swap the dictionary files → `cargo xtask gen-dict` → commit; the dictionary version is annotated as a constant in `radicom-dict` (e.g. `2026b`).

## 10. Performance Design Notes

1. Parsing hot path: first-byte VR lookup table; tag interning index (TAG-04) avoids per-element allocations; lazy numeric/date parsing (ELEM-05).
2. Zero-copy frame access: uncompressed frames are Bytes slices; encapsulated frame aggregation locates data directly via the offset table.
3. Write hot path: the length calculator fixes sizes in a single pass (reused for DICOMDIR offsets); large values stream through `copy_to` in 1 MB chunks.
4. Network: a single writer for the PDU queue; large datasets stream into PDVs (NET-06) without full buffering; C-STORE receive spills to temp files.
5. 16-bit rendering (P2): precomputed LUT lookup (REND-02).
6. Benchmarks: a `criterion` suite mirroring fo-dicom.Benchmark (parsing, JSON, multi-frame render/transcode).

## 11. Test Strategy (INF-06)

1. Unit + integration: full coverage of the data model and IO; test data organized after fo-dicom `Tests/FO-DICOM.Tests/Data` (explicit/implicit, big endian, deflate, multi-frame, SR, DICOMDIR, samples per compression syntax).
2. Round-trip: read → write → re-read, compared byte-level/semantically.
3. Tolerance corpus: private-SQ mismatches, UN misjudgment, broken fragments — one test case per fo-dicom historical issue scenario mapped by RD-03.
4. Network: loopback SCU↔SCP integration tests (C-ECHO/STORE/FIND/MOVE); golden PDU codec samples.
5. Layered codec tests: the same transcode tests run under both registries — "empty default + in-repo mock codec (test-only)" and "radicom-codecs injected" (mirroring fo-dicom's multi-DI-collection pattern).
6. Fuzzing: the reader must not panic on random/mutated bytes (cargo-fuzz, run periodically in CI).

## 12. Deliberate Divergences from fo-dicom

| # | fo-dicom today | radicom decision | Rationale |
|---|---|---|---|
| D1 | C-CANCEL (0x0FFF) defined but unimplemented | Implement it (SRV-08, P2) | Cancelling long queries is a real need |
| D2 | No direct transcode for compressed→compressed | The transcode flow reserves a direct-transcode hook (TR-01); the codecs side declares the capability | Avoids decode/encode round trips |
| D3 | Process-global auto-incrementing MessageID | Per-association numbering | Closer to PS3.7 semantics |
| D4 | DicomServer cannot restart after Stop | shutdown allows re-bind | Operations friendly |
| D5 | C-MOVE SCP does not write Move Originator AE/MessageID | Written by default | Required by the standard |
| D6 | DI container (Microsoft.Extensions.DI) | Static injection points (`set_codec_registry` etc.) + trait extension | No unified DI in the Rust ecosystem; keep things explicit |
| D7 | Four platform render backends (WinForms/WPF/ImageSharp/Skia) | A single `image` feature backend + RawImage | Rust is cross-platform; four backends are unnecessary |
| D8 | Legacy APIs like `UniversalMatch`/`NullElement` | Not implemented | Removed in fo-dicom 5.x |
| D9 | ISO 2022 escape handling embedded in DicomEncoding | A standalone `charset` module, unit-testable in isolation | Complexity isolation |
| D10 | Managed codecs bundled in Core (DICOM RLE encode/decode + JPEG Lossless Process 14/14 SV1 decode-only) | **Zero built-in codecs** — every implementation (RLE included) lives in radicom-codecs; the core ships only the trait, params, empty-by-default registry and transcoder | Clean separation: the core stays free of codec implementations and compression code (flate2 for the Deflated *transfer syntax* stays — it is file-level compression, not an image codec); users who need image codecs depend on radicom-codecs |
| D11 | Two JSON backends (System.Text.Json in Core + a Newtonsoft.Json package) | One JSON backend (serde) in `radicom-json` | serde is the Rust standard; maintaining two backends doubles test surface for no benefit |
| D12 | Platform hosting/instrumentation packages (FO-DICOM.AspNetCore, FO-DICOM.Instrumentation) as separate .NET packages | Capabilities planned in-tree as P2 features — HOST-01 (config-driven server hosting helper, framework-agnostic) and OBS-02 (feature-gated OpenTelemetry wiring of the metrics trait); no .NET-style platform packages | Rust applications bring their own runtime; the core stays dependency-free while the capabilities remain available to every user |

## 13. Release Shape

- crates.io: `radicom` (re-exporting dict), `radicom-network`, `radicom-json`; features: `render`, `image-backend`, `anonymizer` (regex), etc.
- `radicom-codecs` ships from its own repository (with native build scripts); its README documents the compatibility matrix.
- MSRV policy: tracks current stable − 2.
