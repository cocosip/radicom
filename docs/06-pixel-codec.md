# Module Spec and Interface Contract: Pixel Data, Codec Interfaces, Transcoder

> Status: implemented (milestone M4; see [03-development-plan.md](03-development-plan.md) §3) — PixelData, palette LUT, overlays, the codec contract/registry and the transcoder are done; codec implementations live in radicom-codecs
> Feature IDs covered: PIX-01..05, CODEC-01..03, CODEC-05, TR-01..03. ~~CODEC-04 (built-in RLE codec)~~ was dropped on 2026-10-27: **radicom ships zero built-in codecs**; every implementation — JPEG family, JPEG-LS, J2K, HTJ2K and DICOM RLE — is provided by radicom-codecs (divergence D10 in [02-architecture.md](02-architecture.md) §12).
> Behavior baseline: fo-dicom `Imaging/DicomPixelData.cs`, `Imaging/Codec/` (IDicomCodec, DicomCodecParams, TranscoderManager, DicomTranscoder), `Imaging/DicomOverlayData.cs`
> **This document is the normative interface contract between radicom and radicom-codecs.** After M4's boundary review it is frozen; changes follow semver-major.

---

## 1. PixelData Facade (PIX-01..03)

### 1.1 Role

`PixelData` is the single read/write gate for pixel data: codecs, the transcoder and the render module all go through it. It binds to a dataset and materializes views over `(7FE0,0010)`; it never touches other elements except the well-known pixel-module tags it exposes as properties.

### 1.2 API

```rust
pub struct PixelData<'a> { /* dataset: &'a mut Dataset, kind: Kind */ }
enum Kind { OtherByte, OtherWord, Encapsulated }

impl<'a> PixelData<'a> {
    pub fn create_new(ds: &mut Dataset) -> Result<PixelData<'_>>;   // chooses kind per syntax & bits
    pub fn from_dataset(ds: &'a Dataset) -> Result<PixelData<'a>>;  // dispatches on existing element kind
    // properties (backed by tags)
    pub fn width(&self) -> u32;                    // (0028,0011) Columns
    pub fn height(&self) -> u32;                   // (0028,0010) Rows
    pub fn number_of_frames(&self) -> u32;         // (0028,0008), default 1
    pub fn bits_allocated(&self) -> u16;           // (0028,0100), validated 1|8|16|32|64
    pub fn bits_stored(&self) -> u16;              // (0028,0101)
    pub fn high_bit(&self) -> u16;                 // (0028,0102)
    pub fn samples_per_pixel(&self) -> u16;        // (0028,0002)
    pub fn pixel_representation(&self) -> u16;     // (0028,0103) signed
    pub fn planar_configuration(&self) -> u16;     // (0028,0006)
    pub fn photometric_interpretation(&self) -> Option<PhotometricInterpretation>;
    pub fn transfer_syntax(&self) -> &TransferSyntax;
    // frames
    pub fn get_frame(&self, index: u32) -> Result<ByteBuffer>;
    pub fn add_frame(&mut self, data: impl AsRef<[u8]>) -> Result<u32>;   // returns new frame index
    pub fn uncompressed_frame_size(&self) -> u64;
    // palette (PIX-04)
    pub fn palette_color_lut(&self) -> Option<Vec<Color32>>;
}
```

### 1.3 `create_new` kind selection

| Condition | Kind |
|---|---|
| Syntax encapsulated | `Encapsulated` (fragment sequence); bits_allocated > 16 → `Error::Pixel` |
| Not encapsulated, bits_allocated ∈ {8, 1} or odd-compatible | `OtherByte` |
| Not encapsulated, bits_allocated ∈ {16, 32, 64} | `OtherWord` |

### 1.4 Frame extraction algorithm (PIX-02)

Uncompressed: `Range` view of `[index * uncompressed_frame_size, +size)` over the element buffer — zero copy; endianness normalized with an `Endian` wrapper when the syntax/GE-swap requires.

Encapsulated, in order:
1. Exactly one fragment total → that fragment is the whole (single-frame) data.
2. `fragments.len() == number_of_frames` (offset table excluded) → 1:1 mapping, return fragment `index`.
3. Otherwise use the **basic offset table**: `offset = table[index]`, `next = table[index+1] or total_size`; validate `offset % 2 == 0` and bounds; the frame = the slice spanning fragments from byte-offset `offset` to `next` (a `Composite` view when spanning fragments). Trailing empty fragment tolerated and dropped (fo-dicom GH-1586).
4. Neither holds → `Error::Pixel(VariableLengthWithoutOffsetTable)`.

### 1.5 `add_frame` rules

Append fragment/buffer; pad to even length; increment `(0028,0008)`; for encapsulated: extend the offset table by the appended byte length; if any offset would exceed `u32::MAX`, clear the offset table entirely (readers fall back to rule 5.1.4 case 2).

### 1.6 `uncompressed_frame_size` special cases (PIX-03)

```
base = ceil(columns * rows * bits_allocated / 8) * samples_per_pixel
bits_allocated == 1            → ceil(columns/8) * rows            (row packing)
YBR_*_422 / YBR_*_420          → width rounded up to even before factor math
uncompressed YBR_FULL_422      → samples_per_pixel treated as 2    (chroma subsampling)
```

### 1.7 Overlay access (PIX-05)

`OverlayData::from_dataset(ds) -> Vec<OverlayData>`: candidate groups 0x6000..0x60FF even; a group qualifies when `(gggg,0010)` exists, Rows/Columns non-zero and Type non-empty. Properties map the 60xx tags (rows/columns/type/bits/bit-position/origin/frame-origin/number-of-frames/label/description/data). `load()`:
1. If `(60xx,3000)` present → use it.
2. Else **embedded overlay**: requires an uncompressed syntax (encapsulated → `Error::Pixel(OverlaysRequireDecompression)`); extract the bit plane at `OverlayBitPosition` from frame 0 (8/16-bit paths; >16 bits unsupported; bit position inside the stored-value bits ⇒ treat as not embedded, fo-dicom #110).

## 2. Codec Interface (CODEC-01/02) — the contract

### 2.1 Trait

```rust
pub trait Codec: Send + Sync {
    fn name(&self) -> &str;                          // human-readable, usually the TS name
    fn transfer_syntax(&self) -> &'static TransferSyntax;
    fn default_params(&self) -> CodecParams;
    /// src: decoded-as-needed source frames (get_frame); dst: pre-configured output
    /// (syntax, bits, photometric already set on the dataset). Implementations must:
    ///  - read every frame via src.get_frame(i), i in 0..src.number_of_frames()
    ///  - write every frame via dst.add_frame(...)
    ///  - never touch dataset elements other than through PixelData property setters
    ///  - map all failures to Error::Codec (never panic)
    fn encode(&self, src: &PixelData, dst: &mut PixelData, params: &CodecParams) -> Result<()>;
    fn decode(&self, src: &PixelData, dst: &mut PixelData, params: &CodecParams) -> Result<()>;
}
```

Implementations live in radicom-codecs only (JPEG family, JPEG-LS, J2K, HTJ2K and DICOM RLE). The core contains no codec implementations. CODEC-05 (JPEG SOF bit-depth scanning) remains in the core as a codec-support utility on the shared boundary surface.

### 2.2 Parameters (CODEC-02)

```rust
pub enum CodecParams { None, Jpeg(JpegParams), Jpeg2000(J2kParams), HtJpeg2000(HtJ2kParams), JpegLs(JpegLsParams) }

pub struct JpegParams    { pub quality: u32 /*0-100, def 90*/, pub smoothing_factor: u32 /*def 0*/,
                           pub convert_colorspace_to_rgb: bool /*def false*/,
                           pub sample_factor: SampleFactor /*444|422*/,
                           pub predictor: u32 /*def 1*/, pub point_transform: u32 /*def 0*/ }
pub struct J2kParams     { pub irreversible: bool /*def true: 9/7 vs 5/3*/, pub rate: u32 /*def 20*/,
                           pub rate_levels: Vec<u32>, pub allow_mct: bool /*def true*/,
                           pub update_photometric_interpretation: bool,
                           pub encode_signed_as_unsigned: bool }
pub struct HtJ2kParams   { pub irreversible: bool, pub decompositions: u32 /*def 5*/,
                           pub employ_color_transform: bool,
                           pub progression_order: ProgressionOrder /*LRCP|RLCP|RPCL|PCRL|CPRL*/,
                           pub insert_tlm_markers: bool }
pub struct JpegLsParams  { /* near_lossless error, interleave, color transform, preset coding params;
                              opaque to the core: fields finalized by radicom-codecs; the core only
                              carries the struct */ }
```

Every params struct: `Default` impl matching the listed defaults; unknown enum variants rejected at parse time.

## 3. Registry and Injection (CODEC-03)

```rust
pub trait CodecRegistry: Send + Sync {
    fn has_codec(&self, syntax: &TransferSyntax) -> bool;
    fn can_transcode(&self, from: &TransferSyntax, to: &TransferSyntax) -> bool; // both sides non-encapsulated OR codec present
    fn get_codec(&self, syntax: &TransferSyntax) -> Result<Arc<dyn Codec>>;      // Err(Codec::NotRegistered)
}
pub fn set_codec_registry(r: Arc<dyn CodecRegistry>);   // global, idempotent replace
pub fn codec_registry() -> Arc<dyn CodecRegistry>;      // default is EMPTY — the core ships zero codecs (D10)
```

The default registry contains **no codecs**: without an injected registry, any transcode/decode touching an encapsulated syntax fails fast with `Error::Codec(NotRegistered)` naming the syntax. No reflection/discovery — explicit registration only (AOT-friendly, mirroring fo-dicom 5.x's decision). radicom-codecs ships `NativeCodecRegistry` implementing this trait (JPEG family, JPEG-LS, J2K, HTJ2K **and RLE**); user code calls `radicom::pixel::set_codec_registry(Arc::new(NativeCodecRegistry::new()))` once at startup.

## 4. DICOM RLE Format Reference (implemented by radicom-codecs)

The RLE codec is **not** implemented in radicom. This section records the wire format so the radicom-codecs implementation and its tests can be validated against the same rules (PS3.5 Annex A.4):

- 64-byte header: segment count `n` (u32 LE) followed by `n` segment offsets (u32 LE, relative to the start of the header);
- each segment encodes one color/sample plane (MSB-first byte order), split per the DICOM segmentation recipe; ≤15 segments;
- segment encoding is PackBits-style run-length: literal runs ≤128, repeat runs ≤128 with offset+2 bias;
- decoder target layout follows the destination's planar configuration (planar or interleaved);
- error cases: segment count 0 or >15, truncated segment, offset out of range.

## 5. Transcoder (TR-01..03)

### 5.1 Construction

`Transcoder::new(from, to, in_params, out_params)`; codecs resolved lazily through the global registry; missing codec → `Error::Codec(NotRegistered)` naming the syntax. Default input params: JPEG Process 1 / Process 2&4 decode uses `convert_colorspace_to_rgb = true` (libjpeg YBR output convention).

### 5.2 `transcode_dataset(ds)` decision table

| From → To | Flow |
|---|---|
| No pixel data element | clone; set internal syntax; recalc group lengths |
| Uncompressed → Uncompressed | frame-wise `get_frame/add_frame` direct copy (preserves OB/OW and byte order; **no codec involved**) |
| Compressed → Uncompressed | decode via `from` codec into a new EVLE dataset |
| Uncompressed → Compressed | encode via `to` codec |
| Compressed → Compressed | decode to EVLE then encode via `to` codec; a `direct_transcode` hook (§5.5) is consulted first |

### 5.3 Post-encode bookkeeping

When the output syntax is lossy:
- set `(0028,2110) LossyImageCompression = "01"`;
- append the compression method to `(0028,2114)`;
- compute `(0028,2112) LossyImageCompressionRatio` = round(original_first_frame_bytes / encoded_first_frame_bytes, 3), appended per compression generation.

Always: extract embedded overlays (§5.4), update `(7FE0,0010)` VR semantics (encapsulated ⇒ OB), recalculate group lengths, and for `transcode_file` update `FileMeta.TransferSyntax` + dataset internal syntax.

### 5.4 Embedded overlay extraction

For each 60xx group lacking `(60xx,3000)` but with overlay-shape tags: after decompression extract the bit plane (PIX-05 rules) into a standalone `(60xx,3000)` OW/OB element and normalize `(60xx,0100) OverlayBitsAllocated = 1`, `(60xx,0102) OverlayBitPosition = 0`. `extract_overlays(ds)` is also public standalone (used by the render entry).

### 5.5 Direct transcode hook (divergence D2)

```rust
pub trait DirectTranscoder: Send + Sync { fn can(&self, from: &TransferSyntax, to: &TransferSyntax) -> bool;
                                           fn transcode(&self, src: &PixelData, dst: &mut PixelData) -> Result<()>; }
```
Optional registry-side capability; the Transcoder queries it before falling back to decode+encode. The default registry provides none; radicom-codecs may register some (e.g. lossless→lossless re-containerization). Semantics: the hook owns the frame loop; bookkeeping (§5.3) still applies.

### 5.6 Single-frame decoding (TR-02)

`decode_frame(ds, frame)`: uncompressed → `get_frame(frame)`; encapsulated → clone the dataset, build a single-frame PixelData (frame `frame`'s data as its only fragment), run decode, return frame 0's buffer. The source dataset is never mutated. `decode_pixel_data(ds, frame)` additionally builds the render-side pixel representation (P2; ships with the render feature).

### 5.7 Convenience (TR-03)

`DicomFile::clone_with_syntax(file, syntax, params) -> Result<DicomFile>` and the dataset variant = transcode + full clone semantics.

## 6. Boundary Guarantees and Versioning

**radicom guarantees (semver-stable after M4 freeze):**
1. `Codec`, `CodecParams` (+ all param structs), `DirectTranscoder` traits/types as specified here;
2. `PixelData` public API (§1.2) and its documented behavior — including that `get_frame` returns buffers already endianness-normalized;
3. `CodecRegistry` + `set_codec_registry`/`codec_registry`;
4. `TransferSyntax::register` for any custom syntax codecs need;
5. `Error::Codec` as the single error channel; no panics across the boundary;
6. `ByteBuffer` trait stable enough for codecs to construct `Memory` buffers for `add_frame`.

**radicom-codecs obligations:** implement the traits for the full supported set (at least the fo-dicom.Codecs set: RLE, JPEG Baseline/Extended/Lossless14/SV1, JPEG-LS lossless/near-lossless, JPEG 2000 lossless/lossy, HTJ2K); register every supported syntax explicitly in its registry; never assume access to dataset internals beyond PixelData; CI compatibility matrix against radicom minors.

**Change process:** any change to §2–§4 or §6 of this document is a boundary review: bumped major version, coordinated radicom-codecs release, changelog entry in both repos.

## 7. Test Requirements Checklist

- [ ] PixelData: kind selection matrix; frame extraction cases 1/2/3/4 (incl. composite spanning and GH-1586 trailing fragment); add_frame offset-table growth and overflow clear; frame-size special cases (1-bit, YBR odd width, YBR_FULL_422).
- [ ] Palette LUT: descriptor size-0 → 65536; 16-into-8 truncation; 8-bit entries.
- [ ] Overlays: standalone data; embedded 8/16-bit extraction; embedded under encapsulated → error; #110 bit-position rule.
- [ ] RLE format reference (§4) is validated by radicom-codecs tests (golden vectors incl. fo-dicom RLE test data, multi-sample planar/interleaved, segment-count edges 1 and 15, corruption errors) — tracked in the codecs project, not here.
- [ ] Transcoder: all 5 decision rows verified with the in-repo test-only mock codec; lossy bookkeeping (ratio precision, appended-not-replaced values); overlay extraction on transcode; single-frame decode isolation (source untouched); missing-codec paths produce `NotRegistered` naming the syntax.
- [ ] Registry: default is empty (every encapsulated transcode fails fast with a typed error); injection replaces globally; `can_transcode` truth table.
- [ ] Boundary integration: the mock codec crate (in-repo, test-only) implementing the trait proves the contract compiles and behaves from an external crate.
