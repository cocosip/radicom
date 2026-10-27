# Module Spec: Rendering (DicomImage, Pipelines, LUTs, Pixel Types, Geometry, MPR)

> Status: design phase (milestone M8, priority P2; see [03-development-plan.md](03-development-plan.md) §3)
> Feature IDs covered: REND-01..11
> Behavior baseline: fo-dicom `Imaging/` (DicomImage, GrayscaleRenderOptions, Render/, LUT/, Reconstruction/, Mathematics/, Algorithms/, DicomIconImage, SpatialTransform) — all behaviors below mirror fo-dicom semantics
> Depends on: [04-data-model.md](04-data-model.md) (dataset reads), [06-pixel-codec.md](06-pixel-codec.md) (PixelData facade, `decode_pixel_data`), feature `render` in the core crate

---

## 1. High-Level Entry: `DicomImage` (REND-01)

### 1.1 Construction

```rust
pub struct DicomImage { /* dataset ref or owned clone, per-frame render caches, pipeline state */ }
impl DicomImage {
    pub fn from_dataset(ds: &Dataset) -> Result<Self>;
    pub fn from_dataset_frame(ds: &Dataset, frame: u32) -> Result<Self>;
    pub fn open(path: impl AsRef<Path>) -> Result<Self>;
}
```

Construction steps (fo-dicom order):
1. `Transcoder::extract_overlays(ds)` — lift embedded overlays out of pixel data (06 §5.4) so they survive decompression.
2. PhotometricInterpretation inference when absent (ACR-NEMA era files): samples ≤ 1 → palette LUT present ⇒ `PALETTE COLOR` else `MONOCHROME2`; samples > 1 ⇒ assume `RGB`.

### 1.2 Properties (delegated to the current frame's grayscale pipeline where applicable)

| Property | Behavior |
|---|---|
| `width/height/number_of_frames/is_grayscale` | from PixelData |
| `current_frame` | frame index the window settings apply to |
| `window_center/window_width` | setters rebuild the VOI LUT of the current frame; if `auto_apply_lut_to_all_frames` (default **false**), apply to all frames that don't carry their own values |
| `use_voi_lut` | prefer `(0028,3010)` VOI LUT Sequence over window center/width |
| `invert` | appends InvertLUT to the chain |
| `grayscale_colormap` | 256-entry output color table; MONOCHROME1 default = descending gray, MONOCHROME2 = ascending |
| `scale(f)` | render-time scaling factor; changing it clears all render caches |
| `show_overlays/overlay_color` | overlay compositing toggle; default overlay color magenta `0xFF_FF00FF` |
| `cache_mode` | `CacheType` bit flags: `PIXEL_DATA / LOOKUP_TABLES / DISPLAY / ALL / NONE` — controls which of the three cache tiers are kept |

### 1.3 `render_image(frame) -> Image` flow

```
1. pixel tier  : get IPixelData for frame
                 (uncompressed → PixelDataFactory directly; encapsulated → Transcoder::decode_pixel_data,
                  single-frame isolation, result cached per cache_mode)
2. rescale     : if scale != 1.0 → bilinear (grayscale/color) or nearest-neighbor (1-bit) rescale, cached
3. pipeline    : choose per PI:
                 MONOCHROME1/2          → GenericGrayscalePipeline(GrayscaleRenderOptions::from_dataset(ds, frame))
                 RGB / YBR_FULL / YBR_FULL_422 / YBR_PARTIAL_422 / YBR_ICT / YBR_RCT → RgbColorPipeline
                 PALETTE COLOR          → PaletteColorPipeline(pixelData)
4. overlay     : filter overlays (skip empty buffers; skip Data.Size*8 < rows*cols; frame range via
                 OriginFrame/NumberOfFrames), OR-composite with overlay_color at OriginX/Y-1 offset
5. output      : render pixels through the pipeline LUT → Image backend (REND-07); flip/rotate via
                 SpatialTransform state
```

Three cache tiers: pixels (`IPixelData`), pipeline (LUT chain), rendered output — each invalidated independently by its own setters.

### 1.4 Icon images (REND-10)

`IconImage::try_from_sequence_item(item)` for `(0000,9113)` IconImageSequence items: PI restricted to MONOCHROME1/2/PALETTE COLOR; NumberOfFrames forced to 1; handles the case where the parent dataset is encapsulated but the icon's own pixel data is stored native (parse the icon item independently). Rendering goes through the same pipeline selection.

## 2. Pipelines (REND-02)

```rust
pub trait Pipeline { fn lut(&self) -> Option<&dyn Lut>; fn clear_cache(&mut self); }
```

| Pipeline | LUT chain |
|---|---|
| `GenericGrayscalePipeline` | Modality LUT (sequence form if `(0028,3000)` present, else rescale) → VOI (sequence form when `use_voi_lut` and `(0028,3010)` present, else window function LUT) → Output LUT (256-entry colormap; MONOCHROME1 = reversed table) → optional InvertLUT |
| `RgbColorPipeline` | no LUT (RGB pixels assemble 0xFF000000 \| r<<16 \| g<<8 \| b directly; an LUT, if supplied, is applied per channel) |
| `PaletteColorPipeline` | PaletteColorLUT built from the palette descriptors + first-value-mapped |

**16-bit key optimization**: `pipeline.lut()` wraps the CompositeLUT in a `PrecalculatedLUT` over `[bit_depth.min_value ..= max_value]` so rendering is an O(1) table lookup per pixel. Any setter (window, colormap, invert) drops and lazily rebuilds the chain under a lock.

## 3. LUT Family (REND-03)

```rust
pub trait Lut { fn is_valid(&self) -> bool; fn min_output(&self) -> i32; fn max_output(&self) -> i32;
                fn get(&self, value: f64) -> i32; fn recalculate(&mut self); }
```

| LUT | Mapping (source: standard clause) |
|---|---|
| `VoiLinearLut` | DICOM C.11.2.1.3.1: `((v - (c - 0.5)) / (w - 1) + 0.5) * 255`, clamped; width < 1 treated as ≤1 |
| `VoiLinearExactLut` | C.11.2.1.3.2: linear without the −0.5/−1 adjustments |
| `VoiSigmoidLut` | `255 / (1 + e^(−4·((v − c)/w)))` |
| `VoiSequenceLut` | `(0028,3010)` item: LUTDescriptor (entries = 0 ⇒ 65536; first value mapped) + LUTData (US/SS/OW); out-of-range ⇒ nearest endpoint |
| `ModalityRescaleLut` | `v · slope + intercept` from RescaleSlope/Intercept |
| `ModalitySequenceLut` | `(0028,3000)` item; negative descriptor entry count ⇒ take absolute; data typed by VR |
| `OutputLut` | 0..255 → colormap entry |
| `PaletteColorLut` | palette index → Color32 (see PIX-04 extraction rules) |
| `InvertLut` | `max − v` |
| `PaddingLut` | pixel-padding value → min (not auto-chained; available to pipelines later) |
| `CompositeLut` | sequential composition; output range from the last stage |
| `PrecalculatedLUT` | any LUT materialized over an integer range, endpoint clamp outside |

## 4. GrayscaleRenderOptions (REND-04)

```rust
pub struct GrayscaleRenderOptions { bit_depth: BitDepth, rescale_slope: f64, rescale_intercept: f64,
    voi_lut_function: VoiFunction /*LINEAR|LINEAR_EXACT|SIGMOID*/, modality_lut: Option<ModalityLut>,
    voi_lut_sequence: Option<VoiLutSeqItem>, use_voi_lut: bool,
    window_width: f64, window_center: f64, color_map: [Color32; 256], invert: bool }
```

Factory chain — `from_dataset(ds, frame)` resolves in this priority order:
1. top-level WindowCenter/WindowWidth (multi-window: first pair);
2. else enhanced-MR style functional groups for that frame (`functional_groups(frame)`, DS-06);
3. else Smallest/LargestImagePixelValue tags;
4. else `from_min_max` — decode frame 0, compute min/max over non-padding pixels (PixelPaddingValue filtered, values mapped through the modality LUT first).

Other factories: `from_window_level`, `from_functional_window_level`, `from_image_pixel_value_tags`, `from_min_max`, `from_bit_range`, `from_histogram(ds, percent=90)` (window covering the central `percent` of the histogram).

Normalization rules applied by all factories:
- `LINEAR` with window_width < 1 → switch to `LINEAR_EXACT` (fo-dicom #1905);
- ModalityLUTSequence present ⇒ slope = 1 / intercept = 0 (the sequence LUT takes over);
- VOILUTSequence present ⇒ `use_voi_lut = true`;
- XA/XRF (and retired dual-plane) SOP classes: the modality LUT is **not** applied for display (Clunie guidance);
- MONOCHROME1 seeds the inverted grayscale colormap, MONOCHROME2 the ascending one.

## 5. Pixel Render Types (REND-05)

```rust
pub trait IPixelData { fn width/height/components; fn get_min_max(&self, use_padding: bool) -> (f64,f64);
                       fn get_pixel(&self, x, y) -> f64; fn rescale(&self, scale: f64) -> Box<dyn IPixelData>;
                       fn render(&self, lut: Option<&dyn Lut>, out: &mut [u32]); fn histogram(&self, channel) -> Histogram; }
```

| Type | Construction notes |
|---|---|
| `GrayscalePixelDataU8` | direct bytes |
| `GrayscalePixelDataU16/S16` | shift `<< (bits_allocated − high_bit − 1)` then `>> (bits_allocated − bits_stored)` at construction — the value is normalized to the low bits; S16 uses arithmetic shift to keep the sign |
| `GrayscalePixelDataU32/S32` | same rule; histogram unsupported |
| `SingleBitPixelData` | 1-bit expanded to 0/1 bytes; MultiframeSingleBitSecondaryCapture SOP class reverses bit order (LSB→MSB) |
| `ColorPixelData24` | RGB interleaved; render emits `0xFF000000 \| r<<16 \| g<<8 \| b`; per-channel LUT pass when a LUT is supplied |

`render` parallelizes by row (rayon or std threads — implementation choice, deterministic output).

**PixelDataFactory** (`create(pixel_data, frame) -> Box<dyn IPixelData>`): dispatch by PI + bit depth + signedness + SOP-class special cases; RGB/YBR inputs are first normalized: planar → interleaved, then YbrFull / YbrFull422 / YbrPartial422 → RGB (YBR_FULL_422 + planar is an error). PI == None falls back to the §1.1 ACR-NEMA inference.

## 6. Color Conversion and Primitives (REND-06, REND-11)

- `PixelDataConverter`: `interleaved_to_planar_24` / `planar_to_interleaved_24`; `ybr_full_to_rgb`; `ybr_full_422_to_rgb`; `ybr_partial_422_to_rgb`; `reverse_bits` (1-bit).
- `PhotometricInterpretation::parse`: MONOCHROME1 / MONOCHROME (→ MONOCHROME2) / PALETTE COLOR / RGB / YBR_FULL / YBR_FULL_422 / YBR_PARTIAL_422 / YBR_PARTIAL_420 / YBR_ICT / YBR_RCT, each carrying is_color/is_palette/is_ybr/color_space metadata.
- `Color32` (ARGB), `ColorTable` (MONOCHROME1 descending / MONOCHROME2 ascending 256-entry tables, reverse, load/save 768-byte `.lut` files), `ColorSpace` descriptor (OneBit/Grayscale/Indexed/RGB/BGR/RGBA/YCbCr with per-component subsampling), `BitDepth` (allocated/stored/high_bit/signed, min/max, next-power-of-2), `PlanarConfiguration`, `PixelRepresentation`, `CacheType`.
- `SpatialTransform` (REND-11): scale / rotation (90° steps) / flip_x / flip_y / pan / reset / is_transformed — applied at the output stage.
- Math utilities (REND-11): `Point2/Point3/Vector3`, `Matrix` (3×3 + general), `Rect`, `Interval`, `Histogram` (add/apply_window), `BitList`, `MovingAverage`.
- Interpolation: `BilinearInterpolation` (grayscale 8/16/32 + 24-bit color), `NearestNeighborInterpolation` (1-bit).

## 7. Image Backend (REND-07)

```rust
pub trait Image { fn pixels(&self) -> &[u32]; fn width/height; fn render(&mut self, components, flip_x, flip_y, rotation);
                  fn as_image_rs(self) -> image::DynamicImage /* only with "image" feature */; }
```

- Default backend, zero dependencies: `RawImage` — BGRA32 byte buffer, `as_bytes()`.
- Optional `image` feature: converts to `image::RgbaImage`, enabling PNG/BMP/JPEG export (`save(path)`), used by PRINT-02 and the CLI dump tool (INF-07).

## 8. Geometry and Localizers (REND-08)

`FrameGeometry::from_dataset(ds, frame)`:
- inputs: ImagePositionPatient `(0020,0032)`, ImageOrientationPatient `(0020,0037)`, PixelSpacing `(0028,0030)` — or the per-frame functional-group equivalents for enhanced objects;
- derives row/column unit vectors, normal = row × col; corner coordinates for the four image corners;
- `TransformImagePointToPatient` / `TransformPatientPointToImage` (3×3 matrix + offset);
- `FrameOrientation` classification (Axial/Sagittal/Coronal/Oblique), `FrameGeometryType` (Plane/Volume);
- empty position/orientation arrays yield an invalid-but-non-panicking geometry (fo-dicom #2067).

`ImageLocalizer`: `can_draw_localizer(reference, current)` (non-parallel planes with shared orientation family); `projection_localizer(...)` point set (projection of the current frame onto the reference plane); `intersection_localizer(...)` (crosshair line where the planes intersect).

## 9. MPR Reconstruction (REND-09)

- `ImageData`: one slice = dataset + geometry + `IPixelData`.
- `VolumeData::from_datasets(&[Dataset])` / `from_multiframe(&Dataset)`: stacks slices by geometry; extracts the common attribute set (`get_common_data`) for the output headers.
- `get_cut(top_left, row_dir, col_dir, rows, cols, spacing)`: arbitrary-plane resampling with bilinear interpolation along the slice normal; returns `Slice` (pixels + geometry + min/max; `render_into_byte_array` for 8-bit display).
- `Stack`: resample slices to a fixed spacing along a `StackType` axis.
- `DicomGenerator`: write a `Stack` back as a new series of datasets — new Series/SOP Instance UIDs, Rows/Columns/position/orientation/spacing recomputed, BitsStored inferred from observed max, frames appended via PixelData facade.

## 10. Test Requirements Checklist

- [ ] Pipeline selection table (PI → pipeline); MONOCHROME1 uses the reversed colormap.
- [ ] VOI LUT formulas against the standard's worked examples (linear / linear-exact / sigmoid golden vectors).
- [ ] PrecalculatedLUT: range coverage equals direct evaluation; endpoint clamping.
- [ ] RenderOptions factory chain priority (top-level → functional groups → pixel value tags → min/max); XA/XRF modality-LUT exception; LINEAR→LINEAR_EXACT switch; histogram window.
- [ ] Pixel types: U16/S16 shift normalization golden cases (all bits_allocated/stored/high_bit combos); S16 sign preservation; 1-bit expansion + SOP bit-order special case; ColorPixelData24 render bytes.
- [ ] Color conversions: YBR_FULL/422/PARTIAL_422 → RGB goldens; planar↔interleaved; YBR_FULL_422+planar error.
- [ ] End-to-end render: 16-bit CT and MR samples → PNG compared against fo-dicom-rendered references (pixel-diff within tolerance); enhanced MR per-frame window from functional groups; RT datasets with double rescale values; J2K YBR_RCT input (needs radicom-codecs).
- [ ] Overlays: compositing, frame-range filtering, origin offset, out-of-bounds origin clipped not panicking.
- [ ] Geometry: image↔patient transforms round-trip; localizer line cases (parallel → cannot draw).
- [ ] MPR: cut of a synthetic volume equals analytically expected values; `DicomGenerator` output re-opens and geometry verifies.
- [ ] Icon: valid + invalid PI rejection; encapsulated-parent/native-icon case.
- [ ] SpatialTransform: 90° rotations/flips golden images.
