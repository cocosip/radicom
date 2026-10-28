//! Grayscale rendering and a zero-dependency PNG writer (REND-01/03/04/07;
//! docs/01-architecture.md §10, §3, §4, §7).
//!
//! This round delivers the smallest vertical slice that satisfies the milestone
//! acceptance test — "render a 16-bit CT frame to PNG correctly":
//! modality rescale → VOI (window) LUT → 8-bit grayscale → `RawImage`
//! (BGRA32, REND-07 default backend) → PNG bytes (zlib via the existing flate2,
//! so no new dependency). Colour/palette pipelines, overlays, spatial
//! transforms and the optional `image` crate backend are later REND work.

use std::sync::Arc;

use crate::error::{DataError, Error};
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::model::vr::Vr;
use crate::pixel::overlay::OverlayData;
use crate::pixel::PixelData;

pub mod bit_depth;
pub mod color_table;
pub mod geometry;
pub mod icon;
pub mod interpolation;
pub mod math;
pub mod pixel_converter;
pub mod reconstruction;
pub mod spatial;

pub use color_table::ColorTable;
pub use bit_depth::BitDepth;
pub use pixel_converter::PixelDataConverter;
pub use geometry::{
    can_draw_localizer, intersection_localizer, projection_localizer, FrameGeometry, FrameGeometryType,
    FrameOrientation,
};
pub use icon::IconImage;
pub use interpolation::{bilinear_bgra32, bilinear_color24, bilinear_grayscale, nearest_grayscale};
pub use math::{BitList, Histogram, Interval, Matrix, MovingAverage, Point2, Point3, Rect, Vector3};
pub use reconstruction::{DicomGenerator, ImageData, Slice, Stack, StackType, VolumeData};
pub use spatial::SpatialTransform;

/// Default overlay colour: magenta `0xFF_FF00FF` (docs §1.2).
pub const DEFAULT_OVERLAY_COLOR: crate::pixel::Color32 = crate::pixel::Color32::new(0xFF, 0x00, 0xFF);

/// A parsed LUT table shared by the VOI (`(0028,3010)`) and modality
/// (`(0028,3000)`) LUT sequences (PS3.3 C.11.2.1.1 / C.11.2.1.1): a descriptor
/// `(0028,3002)` plus data `(0028,3006)`, mapping an input value to the entry at
/// `value - first_mapped`, clamped to the table's ends.
#[derive(Debug, Clone)]
pub struct LutTable {
    first_mapped: i32,
    entries: Vec<i32>,
}

impl LutTable {
    /// Builds from a LUT-sequence item's `(0028,3002)` descriptor and
    /// `(0028,3006)` data. Returns `None` when either is missing or malformed.
    pub fn from_item(item: &Dataset) -> Option<LutTable> {
        let desc = element_bytes(item, Tag::LUT_DESCRIPTOR)?;
        if desc.len() < 6 {
            return None;
        }
        let raw_entries = u16::from_le_bytes([desc[0], desc[1]]) as i32;
        // fo-dicom `VOISequenceLUT.GetLUTDescriptor`: a negative entry count
        // "makes no sense" but is accepted as its absolute value
        let count = if raw_entries == 0 { 65536usize } else { raw_entries.unsigned_abs() as usize };
        // first mapped pixel value is SS (signed) per PS3.3
        let first_mapped = u16::from_le_bytes([desc[2], desc[3]]) as i16 as i32;
        let bits = u16::from_le_bytes([desc[4], desc[5]]);
        if !matches!(bits, 8 | 16) {
            return None;
        }
        let data = element_bytes(item, Tag::LUT_DATA)?;
        let data_vr = item.get_element(Tag::LUT_DATA).map(|e| e.vr()).unwrap_or(Vr::Ow);

        let mut out = Vec::with_capacity(count);
        if data.len() == count {
            // 8-bit entries (one byte each) carry the value directly
            out.extend(data[..count].iter().map(|b| *b as i32));
        } else {
            // two bytes per entry (US/OW/SS); an SS table keeps its sign
            if data.len() < count * 2 {
                // fo-dicom: "Number of entries in VOI LUT Sequence do not match"
                return None;
            }
            for i in 0..count {
                let pair = [data[i * 2], data[i * 2 + 1]];
                let v = if data_vr == Vr::Ss {
                    i16::from_le_bytes(pair) as i32
                } else {
                    u16::from_le_bytes(pair) as i32
                };
                out.push(v);
            }
        }
        Some(LutTable { first_mapped, entries: out })
    }

    /// The entry for `value`, clamped to the table's ends; `None` when empty.
    fn at(&self, value: f64) -> Option<i32> {
        if self.entries.is_empty() {
            return None;
        }
        let index = value.round() as i64 - self.first_mapped as i64;
        let clamped = index.clamp(0, self.entries.len() as i64 - 1) as usize;
        Some(self.entries[clamped])
    }

    /// Maps an input value to an 8-bit display byte. fo-dicom's grayscale
    /// pipeline re-windows the LUT output through `VOILinearLUT(min, max)`
    /// using the LUT's first/last entries, so a table that does not span the
    /// full 0..2^n-1 range still reaches white at its top entry.
    pub fn map_byte(&self, value: f64) -> u8 {
        let Some(v) = self.at(value) else {
            return 0;
        };
        let (Some(&first), Some(&last)) = (self.entries.first(), self.entries.last()) else {
            return 0;
        };
        let (min, max) = (first as f64, last as f64);
        let width = max - min;
        let center = (max + min) / 2.0;
        let out = if width < 1.0 {
            linear_exact(v as f64, center, width)
        } else {
            ((v as f64 - (center - 0.5)) / (width - 1.0) + 0.5) * 255.0
        };
        out.round().clamp(0.0, 255.0) as u8
    }

    /// Maps an input value to its raw table value (modality form).
    pub fn map_value(&self, value: f64) -> f64 {
        self.at(value).map(|v| v as f64).unwrap_or(0.0)
    }
}

/// A parsed VOI LUT (`(0028,3010)` item): maps rescaled input values to an
/// 8-bit output byte.
#[derive(Debug, Clone)]
pub struct VoiLut {
    table: LutTable,
}

impl VoiLut {
    pub fn from_item(item: &Dataset) -> Option<VoiLut> {
        LutTable::from_item(item).map(|table| VoiLut { table })
    }

    /// Maps a rescaled input value to an output byte; out-of-range clamps to the
    /// nearest endpoint (PS3.3 C.11.2.1.1).
    pub fn map(&self, value: f64) -> u8 {
        self.table.map_byte(value)
    }
}

/// A parsed modality LUT (`(0028,3000)` item): maps a stored pixel value to a
/// modality-transformed value before the VOI stage.
#[derive(Debug, Clone)]
pub struct ModalityLut {
    table: LutTable,
}

impl ModalityLut {
    pub fn from_item(item: &Dataset) -> Option<ModalityLut> {
        LutTable::from_item(item).map(|table| ModalityLut { table })
    }

    /// Maps a stored value to the modality-transformed value (raw table entry).
    pub fn map(&self, stored: f64) -> f64 {
        self.table.map_value(stored)
    }
}

/// Reads an element's bytes from a dataset (raw buffer form).
fn element_bytes(dataset: &Dataset, tag: Tag) -> Option<Vec<u8>> {
    match dataset.item(&tag)? {
        crate::model::item::DicomItem::Element(e) => {
            e.value().as_buffer().map(|b| b.data().to_vec())
        }
        _ => None,
    }
}

/// VOI LUT function selector (docs §3, §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiFunction {
    /// DICOM C.11.2.1.3.1 — `((v - (c - 0.5)) / (w - 1) + 0.5) * 255`.
    Linear,
    /// C.11.2.1.3.2 — linear without the −0.5/−1 adjustments.
    LinearExact,
    /// `255 / (1 + e^(−4·((v − c)/w)))`.
    Sigmoid,
}

/// Grayscale render settings for one frame (REND-04, docs §4).
#[derive(Debug, Clone)]
pub struct GrayscaleRenderOptions {
    pub rescale_slope: f64,
    pub rescale_intercept: f64,
    pub voi_lut_function: VoiFunction,
    pub window_center: f64,
    pub window_width: f64,
    pub invert: bool,
    /// True when `(0028,3010)` VOI LUT Sequence is present.
    pub use_voi_lut: bool,
    /// Parsed first VOI LUT item, applied in place of the window function when
    /// `use_voi_lut` is set (PS3.3 C.11.2.1.1). Shared behind `Arc` so a
    /// `DicomImage` parses it once and reuses it across frames (§3.3).
    pub voi_lut: Option<Arc<VoiLut>>,
    /// Parsed first modality LUT item `(0028,3000)`; when present it replaces the
    /// rescale slope/intercept at the modality stage (docs §4).
    pub modality_lut: Option<Arc<ModalityLut>>,
}

impl GrayscaleRenderOptions {
    /// Resolves options from a dataset (docs §4 factory chain) for frame 0.
    pub fn from_dataset(dataset: &Dataset) -> Result<Self, Error> {
        Self::from_dataset_frame(dataset, 0)
    }

    /// Resolves options from a dataset for a specific frame (docs §4 chain).
    ///
    /// Priority: a `(0028,3010)` VOI LUT Sequence (else WindowCenter/Width, else
    /// a per-frame functional-group window `(0028,9132)`, else
    /// Smallest/LargestImagePixelValue, else decoded pixel min/max). Rescale comes
    /// from a `(0028,3000)` modality LUT Sequence when present, else
    /// `(0028,1053)`/`(0028,1052)`.
    pub fn from_dataset_frame(dataset: &Dataset, frame: u32) -> Result<Self, Error> {
        let voi_lut = parse_voi_lut(dataset);
        let modality_lut = parse_modality_lut(dataset);
        Self::from_dataset_frame_parts(dataset, frame, voi_lut, modality_lut)
    }

    /// Builds options for `frame` from already-parsed LUT tables. `DicomImage`
    /// parses `voi_lut`/`modality_lut` once at construction and passes them here
    /// per frame, so a multi-frame render never re-parses the LUT (§3.3). The
    /// window is still resolved per frame (a per-frame window depends on it).
    fn from_dataset_frame_parts(
        dataset: &Dataset,
        frame: u32,
        voi_lut: Option<Arc<VoiLut>>,
        modality_lut: Option<Arc<ModalityLut>>,
    ) -> Result<Self, Error> {
        let rescale_intercept = dataset.try_get::<String>(Tag::RESCALE_INTERCEPT).and_then(|s| s.trim().parse().ok()).unwrap_or(0.0);
        let rescale_slope = dataset.try_get::<String>(Tag::RESCALE_SLOPE).and_then(|s| s.trim().parse().ok()).unwrap_or(1.0);

        // A VOI LUT Sequence takes precedence over WindowCenter/Width (docs §4):
        // when its first item is usable, no window is required.
        let (window_center, window_width) = if voi_lut.is_some() {
            (0.0, 0.0)
        } else {
            // Priority 1/2/3 (tags), else 4 (decode the frame and use min/max).
            match resolve_window(dataset) {
                Ok(pair) => pair,
                Err(_) => match Self::from_functional_window_level(dataset, frame) {
                    Ok(pair) => pair,
                    Err(_) => {
                        let mm = Self::from_min_max(dataset, frame)?;
                        (mm.window_center, mm.window_width)
                    }
                },
            }
        };

        // (0028,1056) VOILUTFunction selects the window formula; LINEAR by default.
        // LINEAR with width < 1 still falls back to LINEAR_EXACT at map time (#1905).
        let voi_lut_function = voi_function_from_dataset(dataset);

        let use_voi_lut = voi_lut.is_some();
        let invert = dataset
            .try_get_str(Tag::PHOTOMETRIC_INTERPRETATION)
            .map(|pi| pi.trim() == "MONOCHROME1")
            .unwrap_or(false);

        // A modality LUT Sequence (0028,3000) replaces the rescale slope/intercept
        // at the modality stage (docs §4: the sequence LUT takes over). XA/XRF
        // display follows Clunie guidance: the modality LUT is *not* applied.
        let (rescale_slope, rescale_intercept) = if modality_lut.is_some() {
            (1.0, 0.0)
        } else {
            (rescale_slope, rescale_intercept)
        };

        Ok(GrayscaleRenderOptions {
            rescale_slope,
            rescale_intercept,
            voi_lut_function,
            window_center,
            window_width,
            invert,
            use_voi_lut,
            voi_lut,
            modality_lut,
        })
    }

    /// Options whose window covers the central `percent` of the frame's stored
    /// value histogram (REND-04 `from_histogram`, docs §4). Decodes native 8/16-bit
    /// frame samples to build the histogram; the modality rescale and PI inversion
    /// follow the same rules as `from_dataset`.
    pub fn from_histogram(dataset: &Dataset, frame: u32, percent: i32) -> Result<Self, Error> {
        // Two streaming passes over the sample buffer: one for the bin range, one
        // to fill the histogram. Never materialises the frame as a `Vec` (§3.9).
        // Padding is excluded from the bin range (and cleared below) so a large
        // 0x0000/0xFFFF spike cannot stretch or dominate the derived window.
        let pad = first_number(dataset, Tag::PIXEL_PADDING_VALUE);
        let mut range: Option<(f64, f64)> = None;
        visit_frame_stored_values(dataset, frame, |v| {
            if Some(v) == pad {
                return;
            }
            range = Some(match range {
                Some((lo, hi)) => (lo.min(v), hi.max(v)),
                None => (v, v),
            });
        })
        .map_err(|_| unsupported("histogram window needs native 8/16-bit samples"))?;
        let (min, max) = match range {
            Some((lo, hi)) => (lo.round() as i32, hi.round() as i32),
            None => return Err(unsupported("frame has no samples for a histogram")),
        };
        // Guard the bin array against a 32-bit-stored value domain (a 2^32-bin
        // histogram is not representable); 1/8/16-bit ranges are well under this.
        if (max as i64 - min as i64) > 1_048_576 {
            return Err(unsupported("histogram window range is too wide to bin"));
        }
        let mut histogram = Histogram::new(min, max);
        visit_frame_stored_values(dataset, frame, |v| histogram.add(v.round() as i32))
            .map_err(|_| unsupported("histogram window needs native 8/16-bit samples"))?;
        // fo-dicom clears the padding bin before choosing the window
        // (GrayscaleRenderOptions.cs:505 `histogram.Clear(padding)`).
        if let Some(pad) = pad {
            histogram.clear(pad.round() as i32);
        }
        histogram.apply_window_percent(percent.clamp(1, 100));

        let start = histogram.window_start() as f64;
        let end = histogram.window_end() as f64;
        let center = (start + end) / 2.0;
        let width = (end - start).max(1.0);

        // Same modality/invert rules as from_dataset, but the window is explicit.
        let mut base = GrayscaleRenderOptions::from_window_level(center, width);
        base.apply_dataset_modality_and_invert(dataset);
        Ok(base)
    }

    /// Options whose window is the frame's min/max of non-padding pixels
    /// (REND-04 `from_min_max`, docs §4 priority 4). `PixelPaddingValue` pixels are
    /// filtered; values are mapped through the modality LUT first.
    pub fn from_min_max(dataset: &Dataset, frame: u32) -> Result<Self, Error> {
        let mut base = GrayscaleRenderOptions::from_window_level(0.0, 1.0);
        base.apply_dataset_modality_and_invert(dataset);

        // Stream the non-padding min/max in one pass; no whole-frame `Vec` (§3.9).
        let pad = first_number(dataset, Tag::PIXEL_PADDING_VALUE);
        let mut range: Option<(f64, f64)> = None;
        visit_frame_stored_values(dataset, frame, |v| {
            // filter padding pixels (both the signed and unsigned readings of the tag)
            if Some(v) == pad {
                return;
            }
            range = Some(match range {
                Some((lo, hi)) => (lo.min(v), hi.max(v)),
                None => (v, v),
            });
        })?;
        let (min, max) = match range {
            Some((lo, hi)) => (lo, hi),
            None => return Err(unsupported("frame has no non-padding samples")),
        };
        // map through the modality LUT when present (docs: values mapped first)
        let (min, max) = match &base.modality_lut {
            Some(lut) => (lut.map(min), lut.map(max)),
            None => (
                min * base.rescale_slope + base.rescale_intercept,
                max * base.rescale_slope + base.rescale_intercept,
            ),
        };
        base.window_center = (min + max) / 2.0;
        base.window_width = (max - min).max(1.0);
        Ok(base)
    }

    /// Options from the Smallest/LargestImagePixelValue tags `(0028,0106)`/
    /// `(0028,0107)` (REND-04 `from_image_pixel_value_tags`, docs §4 priority 3).
    /// Errors when either tag is absent.
    pub fn from_image_pixel_value_tags(dataset: &Dataset) -> Result<Self, Error> {
        let (smallest, largest) = match (
            first_number(dataset, Tag::SMALLEST_IMAGE_PIXEL_VALUE),
            first_number(dataset, Tag::LARGEST_IMAGE_PIXEL_VALUE),
        ) {
            (Some(lo), Some(hi)) => (lo, hi),
            _ => return Err(unsupported("no Smallest/LargestImagePixelValue tags")),
        };
        let mut base = GrayscaleRenderOptions::from_window_level(
            (smallest + largest) / 2.0,
            (largest - smallest).max(1.0),
        );
        base.apply_dataset_modality_and_invert(dataset);
        Ok(base)
    }

    /// Options covering the full stored bit range (REND-04 `from_bit_range`,
    /// docs §4): a window over `[0, 2^bitsStored-1]` (unsigned) or
    /// `[-2^(bitsStored-1), 2^(bitsStored-1)-1]` (signed).
    pub fn from_bit_range(dataset: &Dataset) -> Result<Self, Error> {
        let mut owned = dataset.clone();
        let pixel = PixelData::from_dataset(&mut owned)?;
        let bits = match pixel.bits_stored() {
            0 => pixel.bits_allocated()?,
            b => b,
        };
        if bits == 0 || bits > 32 {
            return Err(unsupported("stored bit depth is not renderable"));
        }
        let signed = pixel.pixel_representation() != 0;
        let depth = BitDepth::new(pixel.bits_allocated()?, bits, pixel.high_bit(), signed);
        let (min, max) = (depth.min_value() as f64, depth.max_value() as f64);
        let mut base = GrayscaleRenderOptions::from_window_level((min + max) / 2.0, (max - min).max(1.0));
        base.apply_dataset_modality_and_invert(dataset);
        Ok(base)
    }

    /// A per-frame functional-group window (REND-04 priority 2, docs §4):
    /// flattens enhanced-multiframe functional groups for `frame` (DS-06) and
    /// reads the `(0028,9132)` FrameVOILUTSequence item's WindowCenter/Width,
    /// falling back to top-level values in the flattened set. Errors when absent.
    pub fn from_functional_window_level(dataset: &Dataset, frame: u32) -> Result<(f64, f64), Error> {
        let fg = dataset.functional_groups(frame)?;
        // (0028,9132) FrameVOILUTSequence item carries the per-frame window
        if let Some(seq) = fg.try_get_sequence(Tag::FRAME_VOILUT_SEQUENCE) {
            if let Some(item) = seq.items().first() {
                if let (Some(center), Some(width)) = (
                    first_number(item, Tag::WINDOW_CENTER),
                    first_number(item, Tag::WINDOW_WIDTH),
                ) {
                    return Ok((center, width));
                }
            }
        }
        // some encoders leave the window directly in the flattened set
        match (
            first_number(&fg, Tag::WINDOW_CENTER),
            first_number(&fg, Tag::WINDOW_WIDTH),
        ) {
            (Some(center), Some(width)) => Ok((center, width)),
            _ => Err(unsupported("no functional-group window for this frame")),
        }
    }

    /// Applies the rescale/modality-LUT and inversion rules shared by the
    /// dataset-driven factories (docs §4).
    fn apply_dataset_modality_and_invert(&mut self, dataset: &Dataset) {
        self.rescale_slope = dataset
            .try_get::<String>(Tag::RESCALE_SLOPE)
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1.0);
        self.rescale_intercept = dataset
            .try_get::<String>(Tag::RESCALE_INTERCEPT)
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0.0);
        self.modality_lut = parse_modality_lut(dataset);
        if self.modality_lut.is_some() {
            self.rescale_slope = 1.0;
            self.rescale_intercept = 0.0;
        }
        self.invert = dataset
            .try_get_str(Tag::PHOTOMETRIC_INTERPRETATION)
            .map(|pi| pi.trim() == "MONOCHROME1")
            .unwrap_or(false);
        self.voi_lut_function = voi_function_from_dataset(dataset);
    }

    /// Options from an explicit window (REND-04 `from_window_level`).
    pub fn from_window_level(center: f64, width: f64) -> Self {        GrayscaleRenderOptions {
            rescale_slope: 1.0,
            rescale_intercept: 0.0,
            voi_lut_function: VoiFunction::Linear,
            window_center: center,
            window_width: width,
            invert: false,
            use_voi_lut: false,
            voi_lut: None,
            modality_lut: None,
        }
    }

    /// Maps a stored pixel value to an 8-bit display value through the full
    /// chain: modality (LUT sequence or rescale) → VOI (LUT or window).
    pub fn map(&self, stored: f64) -> u8 {
        let value = match &self.modality_lut {
            // a present modality LUT replaces the rescale slope/intercept
            Some(lut) => lut.map(stored),
            None => stored * self.rescale_slope + self.rescale_intercept,
        };
        self.map_rescaled(value)
    }

    /// Applies only the VOI stage to an already-rescaled value.
    pub fn map_rescaled(&self, value: f64) -> u8 {
        // A present VOI LUT Sequence overrides the window function (docs §4).
        if let Some(lut) = &self.voi_lut {
            let mut out = lut.map(value) as f64;
            if self.invert {
                out = 255.0 - out;
            }
            return out.round().clamp(0.0, 255.0) as u8;
        }
        let mut out = match self.voi_lut_function {
            // width < 1 is treated as <= 1 to avoid a divide-by-zero (docs §3)
            VoiFunction::Linear => {
                let w = self.window_width;
                if w == 1.0 {
                    // fo-dicom VOILinearLUT: a width of exactly one is a
                    // threshold at the centre, not the general formula
                    if value < self.window_center - 0.5 {
                        0.0
                    } else {
                        255.0
                    }
                } else if w < 1.0 {
                    return self.map_rescaled_exact(value);
                } else {
                    ((value - (self.window_center - 0.5)) / (w - 1.0) + 0.5) * 255.0
                }
            }
            VoiFunction::LinearExact => linear_exact(value, self.window_center, self.window_width),
            VoiFunction::Sigmoid => {
                let w = if self.window_width == 0.0 { 1.0 } else { self.window_width };
                255.0 / (1.0 + (-4.0 * ((value - self.window_center) / w)).exp())
            }
        };
        if self.invert {
            out = 255.0 - out;
        }
        out.round().clamp(0.0, 255.0) as u8
    }

    fn map_rescaled_exact(&self, value: f64) -> u8 {
        let out = linear_exact(value, self.window_center, self.window_width);
        let out = if self.invert { 255.0 - out } else { out };
        out.round().clamp(0.0, 255.0) as u8
    }
}

/// Parses the first `(0028,3010)` VOI LUT item into a shared table, if usable.
fn parse_voi_lut(dataset: &Dataset) -> Option<Arc<VoiLut>> {
    dataset
        .try_get_sequence(Tag::VOILUT_SEQUENCE)
        .and_then(|seq| seq.items().first())
        .and_then(VoiLut::from_item)
        .map(Arc::new)
}

/// Parses the first `(0028,3000)` modality LUT item, honouring the XA/XRF
/// rule that display omits it (Clunie guidance, docs §4).
fn parse_modality_lut(dataset: &Dataset) -> Option<Arc<ModalityLut>> {
    if is_xa_xrf(dataset) {
        return None;
    }
    dataset
        .try_get_sequence(Tag::MODALITY_LUT_SEQUENCE)
        .and_then(|seq| seq.items().first())
        .and_then(ModalityLut::from_item)
        .map(Arc::new)
}

/// The `(0028,1056)` VOILUTFunction selector (CS). Missing/unknown → LINEAR.
/// fo-dicom `DicomImage.VOILUTFunction` mapping: SIGMOID / LINEAR_EXACT / LINEAR.
fn voi_function_from_dataset(dataset: &Dataset) -> VoiFunction {
    match dataset.try_get_str(Tag::VOILUT_FUNCTION).as_deref().map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("SIGMOID") => VoiFunction::Sigmoid,
        Some(v) if v.eq_ignore_ascii_case("LINEAR_EXACT") => VoiFunction::LinearExact,
        _ => VoiFunction::Linear,
    }
}

fn linear_exact(value: f64, center: f64, width: f64) -> f64 {
    if width == 0.0 {
        return if value > center { 255.0 } else { 0.0 };
    }
    ((value - center) / width + 0.5) * 255.0
}

/// True for X-Ray Angiographic / X-Ray Radiofluoroscopic Image Storage SOP
/// classes, whose display omits the modality LUT (Clunie guidance, docs §4).
fn is_xa_xrf(dataset: &Dataset) -> bool {
    const XA: &str = "1.2.840.10008.5.1.4.1.1.12.1";
    const XRF: &str = "1.2.840.10008.5.1.4.1.1.12.2";
    dataset
        .try_get_str(Tag::SOP_CLASS_UID)
        .map(|uid| {
            let uid = uid.trim();
            uid == XA || uid == XRF
        })
        .unwrap_or(false)
}

/// Resolves the window centre/width (docs §4 priority 1 and 3).
fn resolve_window(dataset: &Dataset) -> Result<(f64, f64), Error> {
    // 1. top-level WindowCenter/WindowWidth, first pair of a possible multi-value
    if let (Some(center), Some(width)) = (
        first_number(dataset, Tag::WINDOW_CENTER),
        first_number(dataset, Tag::WINDOW_WIDTH),
    ) {
        return Ok((center, width));
    }
    // 3. Smallest/LargestImagePixelValue → a min/max window
    if let (Some(smallest), Some(largest)) = (
        first_number(dataset, Tag::SMALLEST_IMAGE_PIXEL_VALUE),
        first_number(dataset, Tag::LARGEST_IMAGE_PIXEL_VALUE),
    ) {
        let center = (smallest + largest) / 2.0;
        let width = (largest - smallest).max(1.0);
        return Ok((center, width));
    }
    Err(Error::Data(DataError::Other {
        message: "no WindowCenter/WindowWidth or pixel-value range to render".into(),
    }))
}

/// Parses the first value of a numeric (DS/IS) attribute, tolerating the
/// backslash-separated multi-value form.
fn first_number(dataset: &Dataset, tag: Tag) -> Option<f64> {
    // Numeric VRs (US/SS/UL/SL/FL/FD) are byte-backed; string VRs (IS/DS) hold
    // text. Read numeric first so US attributes like PixelPaddingValue work, then
    // fall back to the textual form.
    if let Some(v) = dataset.try_get::<f64>(&tag) {
        return Some(v);
    }
    let text = dataset.try_get_str(tag)?;
    text.split('\\').next()?.trim().parse::<f64>().ok()
}

/// Borrows one native 1/8/16/32-bit frame's byte window plus its [`BitDepth`] and
/// frame dimensions.
///
/// `PixelData::from_dataset` needs `&mut Dataset`, so the dataset is cloned; a
/// read-only pixel accessor in `pixel/` would remove that clone, but it is out
/// of this module's scope (§3.4 ceiling, documented at `render_grayscale_with_options`).
fn frame_sample_buffer(dataset: &Dataset, frame: u32) -> Result<(crate::model::buffer::SharedBuffer, BitDepth, usize, usize), Error> {
    let mut owned = dataset.clone();
    let pixel = PixelData::from_dataset(&mut owned)?;
    let bits = pixel.bits_allocated()?;
    if bits == 64 {
        return Err(unsupported("64-bit grayscale samples are not supported (supported: 1, 8, 16, 32)"));
    }
    if !matches!(bits, 1 | 8 | 16 | 32) || pixel.transfer_syntax().is_encapsulated() {
        return Err(unsupported("frame values need native 1/8/16/32-bit samples"));
    }
    let stored = match pixel.bits_stored() {
        0 => bits,
        s => s.min(bits),
    };
    let (width, height) = (pixel.width() as usize, pixel.height() as usize);
    let signed = pixel.pixel_representation() != 0;
    let depth = BitDepth::new(bits, stored, pixel.high_bit(), signed);
    let buffer = pixel.get_frame(frame)?;
    Ok((buffer, depth, width, height))
}

/// Calls `f` once per stored sample of a native 1/8/16/32-bit frame, without
/// ever materialising the frame as a `Vec` (§3.9). Values are normalised and
/// sign-extended from the stored depth, matching the previous whole-frame
/// `Vec<f64>` result exactly.
fn visit_frame_stored_values(dataset: &Dataset, frame: u32, mut f: impl FnMut(f64)) -> Result<(), Error> {
    let (buffer, depth, width, height) = frame_sample_buffer(dataset, frame)?;
    let raw = buffer.data();
    let raw: &[u8] = raw.as_ref();
    let signed = depth.signed;
    let stored = depth.stored;
    let value = |v: u16| -> f64 {
        let n = depth.normalize(v);
        if signed {
            signed_from_stored(n, stored)
        } else {
            n as f64
        }
    };
    if depth.allocated == 1 {
        for bit in unpack_single_bit(raw, width, height) {
            f(value(bit));
        }
    } else if depth.allocated == 8 {
        for &b in raw {
            f(value(b as u16));
        }
    } else if depth.allocated == 32 {
        for c in raw.as_chunks::<4>().0 {
            f(sample32_value(u32::from_le_bytes([c[0], c[1], c[2], c[3]]), depth));
        }
    } else {
        for c in raw.as_chunks::<2>().0 {
            f(value(u16::from_le_bytes([c[0], c[1]])));
        }
    }
    Ok(())
}

/// Unpacks `width`×`height` one-bit samples packed 8 per byte, most-significant
/// bit first, each row padded to a byte boundary (PS3.3 C.7.6.3.1.1). Reversing
/// each byte turns the MSB-first stream into an LSB-first one for indexing.
fn unpack_single_bit(raw: &[u8], width: usize, height: usize) -> Vec<u16> {
    let flipped = PixelDataConverter::reverse_bits(raw);
    let row_bytes = width.div_ceil(8);
    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        let base = y * row_bytes;
        for x in 0..width {
            let byte = flipped.get(base + x / 8).copied().unwrap_or(0);
            out.push(((byte >> (x % 8)) & 1) as u16);
        }
    }
    out
}

/// Interprets a normalised `stored`-bit sample as a signed value.
fn signed_from_stored(normalized: u16, stored: u16) -> f64 {
    if stored == 0 {
        return normalized as f64;
    }
    if stored >= 16 {
        return (normalized as i16) as f64;
    }
    let sign = 1u16 << (stored - 1);
    if normalized & sign != 0 {
        (normalized as i32 - (1i32 << stored)) as f64
    } else {
        normalized as f64
    }
}

/// A rendered frame: 8-bit grayscale expanded to a BGRA32 buffer (REND-07
/// `RawImage` default backend, docs §7).
#[derive(Debug, Clone)]
pub struct RawImage {
    width: u32,
    height: u32,
    /// BGRA32, row-major, top-left origin.
    bytes: Vec<u8>,
}

impl RawImage {
    pub fn new(width: u32, height: u32, bytes: Vec<u8>) -> Self {
        RawImage { width, height, bytes }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The raw BGRA32 bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The BGRA pixels as `u32` (0xAARRGGBB, matching fo-dicom's Image).
    pub fn pixels(&self) -> Vec<u32> {
        self.bytes
            .as_chunks::<4>().0.iter()
            .map(|p| u32::from_le_bytes([p[0], p[1], p[2], p[3]]))
            .collect()
    }

    /// Remaps each pixel's grey level through `table` (REND-02 OutputLUT /
    /// REND-06 `ColorTable`). The blue channel carries the 8-bit grey value.
    pub fn apply_colormap(&mut self, table: &ColorTable) {
        for px in self.bytes.as_chunks_mut::<4>().0 {
            let c = table.get(px[0]);
            px[0] = c.b;
            px[1] = c.g;
            px[2] = c.r;
            if c.a != 0 {
                px[3] = c.a;
            }
        }
    }

    /// Returns a copy resized by `factor` (fo-dicom `DicomImage.Scale`): the
    /// output dimensions become `round(width*factor) × round(height*factor)`,
    /// bilinearly resampled per colour channel (REND-01/06). A non-positive or
    /// unit factor, or an unchanged size, returns an unmodified clone.
    pub fn scaled(&self, factor: f64) -> RawImage {
        if !factor.is_finite() || factor <= 0.0 || factor == 1.0 {
            return self.clone();
        }
        let out_w = ((self.width as f64 * factor).round() as u32).max(1);
        let out_h = ((self.height as f64 * factor).round() as u32).max(1);
        if out_w == self.width && out_h == self.height {
            return self.clone();
        }
        // resample the render buffer directly: BGRA in, BGRA out, no round trip
        // through an RGB copy (docs §3.6)
        let out = interpolation::bilinear_bgra32(
            &self.bytes,
            self.width as usize,
            self.height as usize,
            out_w as usize,
            out_h as usize,
        );
        RawImage::new(out_w, out_h, out)
    }

    /// Encodes the image as a PNG (8-bit grayscale, no alpha) into `out`.
    ///
    /// Zero-dependency: uses flate2's zlib for the IDAT stream and computes the
    /// CRC32 the PNG format requires. PNG is grayscale because the render
    /// output is 8-bit luminance.
    pub fn to_png(&self) -> Vec<u8> {
        // build an 8-bit grayscale scanline buffer (1 channel)
        let mut raw = Vec::with_capacity((self.width * self.height + self.height) as usize);
        for y in 0..self.height {
            raw.push(0); // filter type 0 (None) per scanline
            for x in 0..self.width {
                let i = ((y * self.width + x) * 4) as usize;
                // BGRA → luminance is already the blue channel (B==G==R for gray)
                raw.push(self.bytes[i]);
            }
        }
        self.encode_png(&raw, 0) // colour type 0 = grayscale
    }

    /// Encodes the image as an 8-bit RGB PNG (3 channels, no alpha).
    ///
    /// Used for colour/palette render output; grayscale output still uses
    /// [`to_png`](Self::to_png) so it stays a compact single-channel PNG.
    pub fn to_png_rgb(&self) -> Vec<u8> {
        let mut raw = Vec::with_capacity((self.width * self.height * 3 + self.height) as usize);
        for y in 0..self.height {
            raw.push(0); // filter type 0 (None)
            for x in 0..self.width {
                let i = ((y * self.width + x) * 4) as usize;
                // BGRA → RGB
                raw.push(self.bytes[i + 2]);
                raw.push(self.bytes[i + 1]);
                raw.push(self.bytes[i]);
            }
        }
        self.encode_png(&raw, 2) // colour type 2 = truecolour RGB
    }

    /// Encodes the image as an uncompressed 24-bit BGR Windows BMP (bottom-up,
    /// rows padded to a 4-byte boundary). Zero-dependency counterpart to
    /// [`to_png`](Self::to_png) for REND-07's "PNG/BMP export".
    pub fn to_bmp(&self) -> Vec<u8> {
        let w = self.width as usize;
        let h = self.height as usize;
        let row_bytes = w * 3;
        let padded_row = (row_bytes + 3) & !3;
        let image_size = padded_row * h;
        let file_size = 54 + image_size;
        let mut out = Vec::with_capacity(file_size);
        // BITMAPFILEHEADER (14 bytes)
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(file_size as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // reserved 1
        out.extend_from_slice(&0u16.to_le_bytes()); // reserved 2
        out.extend_from_slice(&54u32.to_le_bytes()); // pixel-data offset
        // BITMAPINFOHEADER (40 bytes)
        out.extend_from_slice(&40u32.to_le_bytes()); // header size
        out.extend_from_slice(&(self.width as i32).to_le_bytes());
        out.extend_from_slice(&(self.height as i32).to_le_bytes()); // positive → bottom-up
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB (uncompressed)
        out.extend_from_slice(&(image_size as u32).to_le_bytes());
        out.extend_from_slice(&2835i32.to_le_bytes()); // x pixels/metre (~72 dpi)
        out.extend_from_slice(&2835i32.to_le_bytes()); // y pixels/metre
        out.extend_from_slice(&0u32.to_le_bytes()); // colours used
        out.extend_from_slice(&0u32.to_le_bytes()); // important colours
        // pixel data: bottom-up rows, B,G,R per pixel, row padded to 4 bytes
        let pad = padded_row - row_bytes;
        for y in (0..h).rev() {
            for x in 0..w {
                let i = (y * w + x) * 4;
                out.push(self.bytes[i]); // B
                out.push(self.bytes[i + 1]); // G
                out.push(self.bytes[i + 2]); // R
            }
            out.resize(out.len() + pad, 0u8);
        }
        out
    }

    /// Writes the image to `path`, choosing the encoding from the extension:
    /// `.bmp` → BMP; otherwise PNG (single-channel grayscale when every pixel is
    /// grey, else true-colour RGB). Dependency-free `save(path)` (docs §7).
    pub fn save(&self, path: impl AsRef<std::path::Path>) -> Result<(), Error> {
        let path = path.as_ref();
        let is_bmp = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("bmp"));
        let bytes = if is_bmp {
            self.to_bmp()
        } else if self.pixels_are_gray() {
            self.to_png()
        } else {
            self.to_png_rgb()
        };
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// True when every pixel has equal R, G and B (so grayscale PNG is lossless).
    fn pixels_are_gray(&self) -> bool {
        self.bytes.as_chunks::<4>().0.iter().all(|p| p[0] == p[1] && p[1] == p[2])
    }

    /// Wraps filtered scanlines in the PNG container with the given colour type.
    fn encode_png(&self, raw: &[u8], color_type: u8) -> Vec<u8> {
        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&self.width.to_be_bytes());
        ihdr.extend_from_slice(&self.height.to_be_bytes());
        ihdr.extend_from_slice(&[8, color_type, 0, 0, 0]); // 8-bit, color_type, deflate, filter 0, no interlace
        write_chunk(&mut png, b"IHDR", &ihdr);
        let compressed = zlib_compress(raw);
        write_chunk(&mut png, b"IDAT", &compressed);
        write_chunk(&mut png, b"IEND", &[]);
        png
    }

    /// OR-composites one overlay onto this image (docs §1.3 step 4).
    ///
    /// Sets overlay bits to `color`, placed at the overlay's Origin (1-based,
    /// so the top-left pixel lands at `origin - 1`). Pixels outside the image
    /// are clipped, never panicking (docs §10 checklist).
    pub fn composite_overlay(
        &mut self,
        overlay: &OverlayData,
        dataset: &Dataset,
        color: crate::pixel::Color32,
    ) -> Result<(), Error> {
        let bits = overlay.load(dataset)?;
        let bits = bits.data();
        let bits: &[u8] = bits.as_ref();
        let ov_rows = overlay.rows() as usize;
        let ov_cols = overlay.columns() as usize;
        if ov_rows == 0 || ov_cols == 0 {
            return Ok(());
        }
        // a buffer too small to hold the overlay's bit plane is skipped (docs §1.3)
        let row_bytes = ov_cols.div_ceil(8);
        if bits.len() < row_bytes * ov_rows {
            return Ok(());
        }
        // Origin is 1-based; the composite starts at origin-1
        let (origin_x, origin_y) = overlay.origin();
        let base_x = origin_x as isize - 1;
        let base_y = origin_y as isize - 1;

        for oy in 0..ov_rows {
            for ox in 0..ov_cols {
                let set = bits[oy * row_bytes + ox / 8] & (0x01 << (ox % 8)) != 0;
                if !set {
                    continue;
                }
                let px = base_x + ox as isize;
                let py = base_y + oy as isize;
                if px < 0 || py < 0 || px >= self.width as isize || py >= self.height as isize {
                    continue; // clipped
                }
                let i = ((py as u32 * self.width + px as u32) * 4) as usize;
                // BGRA (the render byte layout) — OR-composite like fo-dicom's
                // OverlayGraphic, so the underlying pixel's other bits survive
                self.bytes[i] |= color.b;
                self.bytes[i + 1] |= color.g;
                self.bytes[i + 2] |= color.r;
                self.bytes[i + 3] |= 0xFF;
            }
        }
        Ok(())
    }
}

/// Writes one PNG chunk: length, type, data, CRC32(type + data).
fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

/// zlib-compresses `input` (2-byte header + deflate + Adler-32), as PNG IDAT.
fn zlib_compress(input: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(input).expect("in-memory zlib write never fails");
    encoder.finish().expect("in-memory zlib finish never fails")
}

/// Minimal CRC-32 (the polynomial PNG uses), to avoid a dependency.
struct Crc32 {
    value: u32,
}

impl Crc32 {
    fn new() -> Self {
        Crc32 { value: 0xFFFF_FFFF }
    }

    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            let mut c = (self.value ^ b as u32) & 0xFF;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            self.value = (self.value >> 8) ^ c;
        }
    }

    fn finish(self) -> u32 {
        self.value ^ 0xFFFF_FFFF
    }
}

/// Renders one frame of `dataset` to an 8-bit `RawImage` using the dataset's
/// window settings (REND-01 `render_image`, docs §1.3 grayscale path).
///
/// Only uncompressed single-sample grayscale frames are handled this round;
/// other inputs return an error rather than a wrong image.
pub fn render_grayscale_frame(dataset: &Dataset, frame: u32) -> Result<RawImage, Error> {
    let options = GrayscaleRenderOptions::from_dataset_frame(dataset, frame)?;
    render_grayscale_with_options(dataset, frame, &options)
}

/// Pixels below this count render on one thread; spawning bands for a small
/// frame costs more than the loop itself (§3.1).
const PARALLEL_MIN_PIXELS: usize = 1 << 17;

/// Renders a grayscale frame with pre-resolved options (lets `DicomImage` apply
/// its own window overrides).
fn render_grayscale_with_options(
    dataset: &Dataset,
    frame: u32,
    options: &GrayscaleRenderOptions,
) -> Result<RawImage, Error> {
    render_grayscale_with_threshold(dataset, frame, options, PARALLEL_MIN_PIXELS)
}

/// As [`render_grayscale_with_options`] with an explicit parallel-size threshold.
/// `usize::MAX` forces the serial path; the byte-identity test uses 0 vs
/// `usize::MAX` to compare the parallel and serial output directly.
fn render_grayscale_with_threshold(
    dataset: &Dataset,
    frame: u32,
    options: &GrayscaleRenderOptions,
    parallel_min_pixels: usize,
) -> Result<RawImage, Error> {
    // PixelData::from_dataset takes &mut Dataset, so a &Dataset render cannot
    // borrow it; the dataset is cloned per render (§3.4). Removing this clone
    // needs a read-only PixelData accessor in `pixel/`, which is outside this
    // module's scope. ponytail: one BTreeMap rebuild per render; add
    // PixelData::from_dataset_ref(&Dataset) and drop the clone.
    let mut owned = dataset.clone();
    let pixel = PixelData::from_dataset(&mut owned)?;
    if pixel.samples_per_pixel() != 1 {
        return Err(unsupported("only single-sample grayscale can be rendered"));
    }
    let bits = pixel.bits_allocated()?;
    if bits == 64 {
        return Err(unsupported("64-bit grayscale samples are not supported (supported: 1, 8, 16, 32)"));
    }
    if !matches!(bits, 1 | 8 | 16 | 32) {
        return Err(unsupported(&format!("bits allocated {bits} is not rendered")));
    }
    if pixel.transfer_syntax().is_encapsulated() {
        return Err(unsupported("compressed frames need the codec layer"));
    }
    let (width, height) = (pixel.width(), pixel.height());
    if width == 0 || height == 0 {
        return Err(unsupported("frame has zero width or height"));
    }

    let buffer = pixel.get_frame(frame)?;
    let raw = buffer.data();
    let raw: &[u8] = raw.as_ref();

    // 1-bit (bilevel) frames are packed 8 pixels per byte, MSB first (PS3.3
    // C.7.6.3.1.1). They are display-black/white, not windowed data.
    if bits == 1 {
        let (w, h) = (width as usize, height as usize);
        let samples = unpack_single_bit(raw, w, h);
        let invert = options.invert;
        let mut bytes = Vec::with_capacity(w * h * 4);
        for v in samples {
            let white = (v != 0) ^ invert;
            let gray = if white { 255u8 } else { 0u8 };
            bytes.extend_from_slice(&[gray, gray, gray, 0xFF]);
        }
        return Ok(RawImage::new(width, height, bytes));
    }

    let signed = pixel.pixel_representation() != 0;
    // Normalise to the low `bits_stored` bits (docs §5) via BitDepth: left-align
    // on the high bit, then drop the sub-bit padding.
    let depth = BitDepth::new(bits, pixel.bits_stored(), pixel.high_bit(), signed);
    let count = (width as usize) * (height as usize);

    // 32-bit samples have a value domain far too large for an occurring-range
    // LUT, so they are mapped per pixel (no 2^32 table). The window/rescale
    // pipeline is unchanged.
    if bits == 32 {
        return render_grayscale_32(raw, width, height, depth, options);
    }

    // Precalculate the window: the mapping is a pure function of the stored
    // value, so evaluate it once per distinct value instead of once per pixel
    // (docs/fo-dicom-alignment/02 §3.2). The table spans only the range that
    // actually occurs, so a 12-bit frame builds 4096 entries, not 65536.
    // Two streaming passes over the sample bytes find min/max and emit output,
    // instead of holding the whole frame in a `Vec<u16>` (§3.9).
    let (mut min, mut max) = (u16::MAX, 0u16);
    for i in 0..count {
        let v = frame_sample_at(raw, i, depth);
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
    }
    let to_grey = |v: u16| -> u8 {
        // the sign handling matches the per-pixel form exactly: reinterpret the
        // allocated width, do not sign-extend from BitsStored
        let stored = if signed {
            if bits == 8 { (v as i8) as f64 } else { (v as i16) as f64 }
        } else {
            v as f64
        };
        options.map(stored)
    };
    let lut: Vec<u8> = (min..=max).map(to_grey).collect();
    let bytes = render_pixel_rows(raw, width, height, depth, &lut, min, parallel_min_pixels);
    Ok(RawImage::new(width, height, bytes))
}

/// Decodes one normalised sample of a native 8/16-bit frame.
fn frame_sample_at(raw: &[u8], index: usize, depth: BitDepth) -> u16 {
    if depth.allocated == 8 {
        depth.normalize(raw.get(index).copied().unwrap_or(0) as u16)
    } else {
        let lo = raw.get(index * 2).copied().unwrap_or(0);
        let hi = raw.get(index * 2 + 1).copied().unwrap_or(0);
        depth.normalize(u16::from_le_bytes([lo, hi]))
    }
}

/// A 32-bit stored value as f64, normalised to the stored bits exactly like
/// fo-dicom `GrayscalePixelDataU32/S32` (left-align on HighBit, then drop the
/// sub-bit padding) and sign-extended when Pixel Representation is 1.
fn sample32_value(value: u32, depth: BitDepth) -> f64 {
    if depth.stored == 0 || depth.stored > 32 || depth.high_bit >= 32 {
        return if depth.signed { (value as i32) as f64 } else { value as f64 };
    }
    let up = (32 - depth.high_bit - 1) as u32;
    let down = (32 - depth.stored) as u32;
    if depth.signed {
        (((value as i32) << up) >> down) as f64
    } else {
        ((value << up) >> down) as f64
    }
}

/// Renders a 32-bit single-sample grayscale frame. Values are mapped per pixel
/// through [`GrayscaleRenderOptions::map`] (no occurring-range LUT, whose domain
/// would be up to 2^32 entries).
fn render_grayscale_32(
    raw: &[u8],
    width: u32,
    height: u32,
    depth: BitDepth,
    options: &GrayscaleRenderOptions,
) -> Result<RawImage, Error> {
    let (w, h) = (width as usize, height as usize);
    let count = w * h;
    if raw.len() < count * 4 {
        return Err(unsupported("frame is shorter than rows×cols×4 bytes"));
    }
    let mut bytes = Vec::with_capacity(count * 4);
    for i in 0..count {
        let v = u32::from_le_bytes([raw[i * 4], raw[i * 4 + 1], raw[i * 4 + 2], raw[i * 4 + 3]]);
        let gray = options.map(sample32_value(v, depth));
        bytes.extend_from_slice(&[gray, gray, gray, 0xFF]);
    }
    Ok(RawImage::new(width, height, bytes))
}

/// Emits the output rows of one band into `dst` (the band's own contiguous
/// slice, row 0 of `dst` = row `rows.start` of the image).
fn emit_rows(
    dst: &mut [u8],
    raw: &[u8],
    depth: BitDepth,
    lut: &[u8],
    min: u16,
    width: usize,
    rows: std::ops::Range<usize>,
) {
    let y0 = rows.start;
    for y in rows {
        let base = (y - y0) * width * 4;
        for x in 0..width {
            let v = frame_sample_at(raw, y * width + x, depth);
            let gray = lut[(v - min) as usize];
            // BGRA with grey in all colour channels, opaque alpha
            let o = base + x * 4;
            dst[o..o + 4].copy_from_slice(&[gray, gray, gray, 0xFF]);
        }
    }
}

/// Renders the pixel rows into a BGRA buffer, splitting the image into per-core
/// row bands with `std::thread::scope` when it is large enough (§3.1). Each band
/// writes a disjoint slice, so the result is byte-identical to the serial path.
fn render_pixel_rows(
    raw: &[u8],
    width: u32,
    height: u32,
    depth: BitDepth,
    lut: &[u8],
    min: u16,
    parallel_min_pixels: usize,
) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut bytes = vec![0u8; w * h * 4];
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(h.max(1));
    if threads <= 1 || w * h < parallel_min_pixels {
        emit_rows(&mut bytes, raw, depth, lut, min, w, 0..h);
        return bytes;
    }
    let rows_per_band = h.div_ceil(threads);
    std::thread::scope(|scope| {
        for (band, chunk) in bytes.chunks_mut(rows_per_band * w * 4).enumerate() {
            let y0 = band * rows_per_band;
            scope.spawn(move || {
                let rows = chunk.len() / (w * 4);
                emit_rows(chunk, raw, depth, lut, min, w, y0..y0 + rows);
            });
        }
    });
    bytes
}

fn unsupported(message: &str) -> Error {
    Error::Data(DataError::Other { message: message.to_string() })
}

/// Renders one frame, choosing the pipeline by photometric interpretation
/// (REND-02, docs §2): MONOCHROME → grayscale window; RGB → colour assembly;
/// PALETTE COLOR → palette index lookup; YBR_FULL / YBR_FULL_422 /
/// YBR_PARTIAL_422 → YCbCr→RGB conversion. Other PIs error.
pub fn render_frame(dataset: &Dataset, frame: u32) -> Result<RawImage, Error> {
    use crate::pixel::photometric::PhotometricInterpretation as Pi;
    let pi = match dataset.try_get_str(Tag::PHOTOMETRIC_INTERPRETATION) {
        Some(s) => Pi::parse(&s),
        // ACR-NEMA files may omit PI; infer from samples + palette (docs §1.1).
        None => Pi::infer_from_dataset(dataset),
    };
    match pi {
        Pi::Monochrome1 | Pi::Monochrome2 => render_grayscale_frame(dataset, frame),
        Pi::Rgb => render_rgb_frame(dataset, frame),
        Pi::PaletteColor => render_palette_frame(dataset, frame),
        Pi::YbrFull => render_ybr_frame(dataset, frame, false, false),
        Pi::YbrFull422 => render_ybr_frame(dataset, frame, false, true),
        Pi::YbrPartial422 => render_ybr_frame(dataset, frame, true, true),
        _ => Err(unsupported("this photometric interpretation is not rendered yet")),
    }
}

/// Renders one frame and composites its overlays in `color` (docs §1.3 step 4).
///
/// Only overlays whose frame range covers `frame` are drawn (frame 0 by
/// default; multi-frame overlays offset by FrameOrigin). Overlays failing to
/// load (e.g. bit position inside stored bits) are skipped, not fatal.
pub fn render_frame_with_overlays(
    dataset: &Dataset,
    frame: u32,
    color: crate::pixel::Color32,
) -> Result<RawImage, Error> {
    let mut image = render_frame(dataset, frame)?;
    composite_overlays(&mut image, dataset, frame, color);
    Ok(image)
}

/// OR-composites every overlay covering `frame` onto `image` in `color`
/// (docs §1.3 step 4). A malformed overlay is skipped, never fatal.
fn composite_overlays(
    image: &mut RawImage,
    dataset: &Dataset,
    frame: u32,
    color: crate::pixel::Color32,
) {
    for overlay in OverlayData::from_dataset(dataset) {
        if !overlay_covers_frame(&overlay, frame) {
            continue;
        }
        if let Err(error) = image.composite_overlay(&overlay, dataset, color) {
            // a malformed overlay must not break the render (docs §10), but it
            // must not vanish silently either: a blank image is not a diagnosis
            tracing::warn!(
                group = format_args!("{:#06X}", overlay.group),
                frame,
                %error,
                "overlay skipped"
            );
        }
    }
}

/// High-level rendering entry point (REND-01, docs §1): wraps a dataset and
/// holds per-render overrides — the current frame, an explicit window that
/// overrides the dataset's own setting, an output `scale`, and a
/// [`SpatialTransform`]. `render` applies all of them and composites overlays.
#[derive(Debug, Clone)]
pub struct DicomImage {
    dataset: Dataset,
    frame: u32,
    window_override: Option<(f64, f64)>,
    colormap: Option<ColorTable>,
    show_overlays: bool,
    overlay_color: crate::pixel::Color32,
    scale: f64,
    transform: SpatialTransform,
    /// Geometry cached at construction so the accessors never re-clone (§3.5).
    width: u32,
    height: u32,
    number_of_frames: u32,
    samples_per_pixel: u16,
    /// LUT tables parsed once at construction and reused for every frame (§3.3).
    voi_lut: Option<Arc<VoiLut>>,
    modality_lut: Option<Arc<ModalityLut>>,
}

impl DicomImage {
    /// Wraps a dataset at frame 0 (docs §1.1).
    pub fn from_dataset(dataset: &Dataset) -> Result<Self, Error> {
        Self::from_dataset_frame(dataset, 0)
    }

    /// Wraps a dataset at a specific frame, verifying pixel data is readable.
    pub fn from_dataset_frame(dataset: &Dataset, frame: u32) -> Result<Self, Error> {
        // One clone: validate and read the geometry from it, then keep it as the
        // owned dataset (the previous code cloned twice, §3.5).
        let mut owned = dataset.clone();
        let (width, height, number_of_frames, samples_per_pixel) = {
            let pixel = PixelData::from_dataset(&mut owned)?;
            (pixel.width(), pixel.height(), pixel.number_of_frames(), pixel.samples_per_pixel())
        };
        Ok(DicomImage {
            dataset: owned,
            frame,
            window_override: None,
            colormap: None,
            show_overlays: true,
            overlay_color: DEFAULT_OVERLAY_COLOR,
            scale: 1.0,
            transform: SpatialTransform::new(),
            width,
            height,
            number_of_frames,
            samples_per_pixel,
            voi_lut: parse_voi_lut(dataset),
            modality_lut: parse_modality_lut(dataset),
        })
    }

    /// Opens a DICOM file and wraps its main dataset.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let file = crate::io::file::DicomFile::open(path)?;
        Self::from_dataset(&file.dataset)
    }

    pub fn width(&self) -> Result<u32, Error> {
        Ok(self.width)
    }

    pub fn height(&self) -> Result<u32, Error> {
        Ok(self.height)
    }

    pub fn number_of_frames(&self) -> Result<u32, Error> {
        Ok(self.number_of_frames)
    }

    pub fn is_grayscale(&self) -> Result<bool, Error> {
        Ok(self.samples_per_pixel == 1)
    }

    /// The frame the default `render` draws.
    pub fn current_frame(&self) -> u32 {
        self.frame
    }

    /// Selects the frame `render` draws.
    pub fn set_frame(&mut self, frame: u32) -> &mut Self {
        self.frame = frame;
        self
    }

    /// Overrides the dataset's own window for grayscale rendering (docs §1.2).
    pub fn set_window(&mut self, center: f64, width: f64) -> &mut Self {
        self.window_override = Some((center, width));
        self
    }

    /// Drops a window override, reverting to the dataset's own setting.
    pub fn clear_window(&mut self) -> &mut Self {
        self.window_override = None;
        self
    }

    /// Render-time scaling factor (docs §1.2 `scale`); 1.0 is unscaled.
    pub fn set_scale(&mut self, scale: f64) -> &mut Self {
        self.scale = scale;
        self
    }

    /// Sets the 256-entry output colormap applied to grayscale output
    /// (REND-02 OutputLUT / REND-06 `ColorTable`); `None` keeps the plain grey
    /// output. Has no effect on colour images.
    pub fn set_colormap(&mut self, colormap: Option<ColorTable>) -> &mut Self {
        self.colormap = colormap;
        self
    }

    /// Turns overlay compositing on/off (docs §1.2 `show_overlays`; default on).
    pub fn set_show_overlays(&mut self, show: bool) -> &mut Self {
        self.show_overlays = show;
        self
    }

    /// Sets the overlay compositing colour (docs §1.2 `overlay_color`; default
    /// magenta `0xFF_FF00FF`).
    pub fn set_overlay_color(&mut self, color: crate::pixel::Color32) -> &mut Self {
        self.overlay_color = color;
        self
    }

    /// Mutable access to the flip/rotate transform applied at the output stage.
    pub fn transform(&mut self) -> &mut SpatialTransform {
        &mut self.transform
    }

    /// Renders the current frame.
    pub fn render(&self) -> Result<RawImage, Error> {
        self.render_frame(self.frame)
    }

    /// Renders `frame` applying the window override, overlays, transform and
    /// scale in order.
    pub fn render_frame(&self, frame: u32) -> Result<RawImage, Error> {
        use crate::pixel::photometric::PhotometricInterpretation as Pi;
        let pi = match self.dataset.try_get_str(Tag::PHOTOMETRIC_INTERPRETATION) {
            Some(s) => Pi::parse(&s),
            None => Pi::infer_from_dataset(&self.dataset),
        };
        // Only true monochrome goes through the window pipeline; palettes are
        // single-sample but must use the palette path (not grayscale windowing).
        let grayscale = matches!(pi, Pi::Monochrome1 | Pi::Monochrome2);

        let mut image = if grayscale {
            // Reuse the LUT tables parsed once at construction (§3.3); only the
            // window is resolved per frame.
            let mut options = GrayscaleRenderOptions::from_dataset_frame_parts(
                &self.dataset,
                frame,
                self.voi_lut.clone(),
                self.modality_lut.clone(),
            )?;
            if let Some((center, width)) = self.window_override {
                // an explicit window supersedes any VOI LUT Sequence
                options.window_center = center;
                options.window_width = width;
                options.use_voi_lut = false;
                options.voi_lut = None;
                options.voi_lut_function = VoiFunction::Linear;
            }
            render_grayscale_with_options(&self.dataset, frame, &options)?
        } else {
            render_frame(&self.dataset, frame)?
        };

        // Output LUT (colormap) applies only to grayscale display, before overlays.
        if grayscale {
            if let Some(table) = &self.colormap {
                image.apply_colormap(table);
            }
        }

        if self.show_overlays {
            composite_overlays(&mut image, &self.dataset, frame, self.overlay_color);
        }
        if self.transform.is_transformed() {
            image = self.transform.apply_to(&image);
        }
        if self.scale != 1.0 && self.scale.is_finite() && self.scale > 0.0 {
            image = image.scaled(self.scale);
        }
        Ok(image)
    }
}

/// True when `overlay` applies to `frame` (docs §1.3 frame-range filtering).
fn overlay_covers_frame(overlay: &OverlayData, frame: u32) -> bool {
    let frame_origin = overlay.frame_origin();
    let first = frame_origin.max(1) as u32 - 1;
    let count = overlay.number_of_frames().max(1);
    frame >= first && frame < first + count
}

/// Reads `count` native samples for a frame, honouring 8/16-bit depth.
///
/// Returns `(samples, width, height)`; samples are raw stored values, in the
/// dataset's sample order (interleaved or planar as stored).
fn read_frame_samples(
    pixel: &PixelData<'_>,
    frame: u32,
) -> Result<(Vec<u16>, u32, u32), Error> {
    let (width, height) = (pixel.width(), pixel.height());
    let bits = pixel.bits_allocated()?;
    if !matches!(bits, 8 | 16) {
        return Err(unsupported(&format!("bits allocated {bits} is not rendered")));
    }
    if pixel.transfer_syntax().is_encapsulated() {
        return Err(unsupported("compressed frames need the codec layer"));
    }
    let buffer = pixel.get_frame(frame)?;
    let raw = buffer.data();
    let raw: &[u8] = raw.as_ref();
    let samples = if bits == 8 {
        raw.iter().map(|b| *b as u16).collect()
    } else {
        raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
    };
    Ok((samples, width, height))
}

/// RgbColorPipeline (docs §2): 3 samples per pixel → BGRA directly.
fn render_rgb_frame(dataset: &Dataset, frame: u32) -> Result<RawImage, Error> {
    let mut owned = dataset.clone();
    let pixel = PixelData::from_dataset(&mut owned)?;
    if pixel.samples_per_pixel() != 3 {
        return Err(unsupported("RGB expects 3 samples per pixel"));
    }
    let planar = pixel.planar_configuration() == 1;
    let bits = pixel.bits_allocated()?;
    let (samples, width, height) = read_frame_samples(&pixel, frame)?;
    let count = (width as usize) * (height as usize);
    if samples.len() < count * 3 {
        return Err(unsupported("frame is shorter than rows×cols×3 samples"));
    }
    // Normalise stored samples to the low BitsStored bits, then scale to 8-bit
    // (grayscale does the same; RGB must too for non-standard depth metadata such
    // as low-aligned 12-bit-in-16 colour, which a bare `>>8` would darken).
    let stored = match pixel.bits_stored() {
        0 => bits,
        s => s.min(bits),
    };
    let depth = BitDepth::new(bits, stored, pixel.high_bit(), false);
    let down = stored.saturating_sub(8);
    let up = 8u16.saturating_sub(stored);
    let to8 = |v: u16| -> u8 {
        let n = depth.normalize(v);
        if stored >= 8 {
            (n >> down) as u8
        } else {
            (n << up) as u8
        }
    };
    let at = |index: usize, channel: usize| -> u16 {
        // planar configuration 1 stores channel planes sequentially
        if planar {
            samples[channel * count + index]
        } else {
            samples[index * 3 + channel]
        }
    };
    let mut bytes = Vec::with_capacity(count * 4);
    for i in 0..count {
        let r = to8(at(i, 0));
        let g = to8(at(i, 1));
        let b = to8(at(i, 2));
        bytes.extend_from_slice(&[b, g, r, 0xFF]);
    }
    Ok(RawImage::new(width, height, bytes))
}

/// PaletteColorPipeline (docs §2): palette index → 256-entry R/G/B LUT.
fn render_palette_frame(dataset: &Dataset, frame: u32) -> Result<RawImage, Error> {
    let mut owned = dataset.clone();
    let pixel = PixelData::from_dataset(&mut owned)?;
    if pixel.samples_per_pixel() != 1 {
        return Err(unsupported("PALETTE COLOR expects 1 sample per pixel"));
    }
    let palette = pixel.palette_color_lut_checked()?;
    // PS3.3 C.7.6.3.1.5: input value v maps to entry (v - first_mapped), and
    // values outside the table clamp to the first/last entry.
    let first_mapped = pixel.palette_first_mapped().unwrap_or(0);
    // Palette indices are stored values: high-aligned depth metadata must be
    // normalised before the lookup, exactly like the RGB and grayscale paths.
    let bits = pixel.bits_allocated()?;
    let stored = match pixel.bits_stored() {
        0 => bits,
        s => s.min(bits),
    };
    let depth = BitDepth::new(bits, stored, pixel.high_bit(), false);
    let (samples, width, height) = read_frame_samples(&pixel, frame)?;
    let count = (width as usize) * (height as usize);
    if samples.len() < count {
        return Err(unsupported("frame is shorter than rows×cols samples"));
    }
    let black = crate::pixel::Color32::new(0, 0, 0);
    let mut bytes = Vec::with_capacity(count * 4);
    for &raw in &samples[..count] {
        let index = depth.normalize(raw);
        let entry = index as i32 - first_mapped;
        let color = if entry < 0 {
            palette.first().copied().unwrap_or(black)
        } else {
            palette.get(entry as usize).copied().or_else(|| palette.last().copied()).unwrap_or(black)
        };
        bytes.extend_from_slice(&[color.b, color.g, color.r, 0xFF]);
    }
    Ok(RawImage::new(width, height, bytes))
}


/// YCbCr → RGB pipelines (docs §2, PS3.3 C.7.6.3.1.2): YBR_FULL (4:4:4 full
/// range), YBR_FULL_422 (4:2:2, full range) and YBR_PARTIAL_422 (4:2:2,
/// studio/limited range). Compressed YBR_ICT/RCT reach here only via the codec
/// layer, so this handles native 8/16-bit 4:4:4 and 4:2:2 data.
fn render_ybr_frame(
    dataset: &Dataset,
    frame: u32,
    partial: bool,
    subsampled: bool,
) -> Result<RawImage, Error> {
    let mut owned = dataset.clone();
    let pixel = PixelData::from_dataset(&mut owned)?;
    // PS3.3 C.7.6.3.1.3: YBR_FULL may be planar (configuration 1); subsampled
    // YBR (4:2:2) is defined only interleaved.
    let planar = pixel.planar_configuration() == 1;
    if planar && subsampled {
        return Err(unsupported("subsampled YBR must be interleaved (planar configuration 0)"));
    }
    if pixel.transfer_syntax().is_encapsulated() {
        return Err(unsupported("compressed YBR frames need the codec layer"));
    }
    let bits = pixel.bits_allocated()?;
    if !matches!(bits, 8 | 16) {
        return Err(unsupported(&format!("bits allocated {bits} is not rendered")));
    }
    let (width, height) = (pixel.width(), pixel.height());
    let buffer = pixel.get_frame(frame)?;
    let raw = buffer.data();
    let raw: &[u8] = raw.as_ref();

    // Read a sample at byte level, honouring 8/16-bit little-endian storage.
    // The stored depth is normalised exactly like the RGB path: PS3.3 defines
    // YBR sample values in 0–255, so high-aligned 12-in-16 (or 10-in-16) data
    // must be shifted down rather than having its high byte taken.
    let stored = match pixel.bits_stored() {
        0 => bits,
        s => s.min(bits),
    };
    let depth = BitDepth::new(bits, stored, pixel.high_bit(), false);
    let down = stored.saturating_sub(8);
    let up = 8u16.saturating_sub(stored);
    let to8 = |v: u16| -> f64 {
        let n = depth.normalize(v);
        if stored >= 8 {
            (n >> down) as f64
        } else {
            (n << up) as f64
        }
    };
    let read = |byte_index: usize| -> f64 {
        if bits == 8 {
            to8(raw[byte_index] as u16)
        } else {
            to8(u16::from_le_bytes([raw[byte_index], raw[byte_index + 1]]))
        }
    };
    let unit = if bits == 8 { 1usize } else { 2usize };
    let count = (width as usize) * (height as usize);

    // Convert one (Y, Cb, Cr) triple to clamped RGB. Cb/Cr are centred on 128
    // (full range) or on the studio midpoint 128 after range expansion (partial).
    let to_rgb = |y: f64, cb: f64, cr: f64| -> (u8, u8, u8) {
        // ponytail: partial-range expansion assumes 8-bit samples (PS3.3 only
        // defines YBR_PARTIAL at 8 bits); 16-bit partial would need its own scale.
        let (y, cb, cr) = if partial {
            // studio-range 8-bit: Y in [16,235], C in [16,240] → expand to full
            let y = ((y - 16.0) * (255.0 / 219.0)).clamp(0.0, 255.0);
            let cb = ((cb - 128.0) * (255.0 / 224.0)).clamp(-128.0, 127.0);
            let cr = ((cr - 128.0) * (255.0 / 224.0)).clamp(-128.0, 127.0);
            (y, cb, cr)
        } else {
            (y, cb - 128.0, cr - 128.0)
        };
        let r = y + 1.402 * cr;
        let g = y - 0.344_136 * cb - 0.714_136 * cr;
        let b = y + 1.772 * cb;
        let cl = |v: f64| v.round().clamp(0.0, 255.0) as u8;
        (cl(r), cl(g), cl(b))
    };

    let mut bytes = Vec::with_capacity(count * 4);
    if subsampled {
        // 4:2:2: per 2 pixels a group of Y1, Y2, Cb, Cr; every row is padded to
        // an even pixel count in the stored data.
        let row_bytes = (width as usize).div_ceil(2) * 4 * unit;
        if raw.len() < row_bytes * height as usize {
            return Err(unsupported("4:2:2 frame is shorter than its subsampled layout"));
        }
        for row in 0..height as usize {
            let row_start = row * row_bytes;
            for col in 0..width as usize {
                let group = col / 2;
                let base = row_start + group * 4 * unit;
                let y = read(base + (col % 2) * unit);
                let cb = read(base + 2 * unit);
                let cr = read(base + 3 * unit);
                let (r, g, b) = to_rgb(y, cb, cr);
                bytes.extend_from_slice(&[b, g, r, 0xFF]);
            }
        }
    } else if planar {
        // 4:4:4 planar (PS3.3 C.7.6.3.1.3): the Y plane, then Cb, then Cr, each
        // holding `count` samples.
        if raw.len() < count * 3 * unit {
            return Err(unsupported("planar YBR frame is shorter than three planes"));
        }
        for i in 0..count {
            let y = read(i * unit);
            let cb = read((count + i) * unit);
            let cr = read((2 * count + i) * unit);
            let (r, g, b) = to_rgb(y, cb, cr);
            bytes.extend_from_slice(&[b, g, r, 0xFF]);
        }
    } else {
        // 4:4:4: interleaved Y, Cb, Cr per pixel
        if raw.len() < count * 3 * unit {
            return Err(unsupported("frame is shorter than rows×cols×3 samples"));
        }
        for i in 0..count {
            let base = i * 3 * unit;
            let y = read(base);
            let cb = read(base + unit);
            let cr = read(base + 2 * unit);
            let (r, g, b) = to_rgb(y, cb, cr);
            bytes.extend_from_slice(&[b, g, r, 0xFF]);
        }
    }
    Ok(RawImage::new(width, height, bytes))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::vr::Vr;

    fn gray_image(width: u16, height: u16, bits: u16, samples: &[u16], center: Option<f64>, width_ds: Option<f64>) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
        ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
        ds.insert_value(Tag::ROWS, height);
        ds.insert_value(Tag::COLUMNS, width);
        ds.insert_value(Tag::BITS_ALLOCATED, bits);
        ds.insert_value(Tag::BITS_STORED, bits);
        ds.insert_value(Tag::HIGH_BIT, bits - 1);
        ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
        if let Some(center) = center {
            ds.insert_str(Tag::WINDOW_CENTER, Vr::Ds, format!("{center}"));
        }
        if let Some(width_ds) = width_ds {
            ds.insert_str(Tag::WINDOW_WIDTH, Vr::Ds, format!("{width_ds}"));
        }
        let mut bytes = Vec::new();
        for &s in samples {
            if bits == 8 {
                bytes.push(s as u8);
            } else {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
        }
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes)),
            ),
        ));
        ds.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
    }

    #[test]
    fn monochrome1_renders_inverted_relative_to_monochrome2() {
        // Same pixels + window, differing PI: MONOCHROME1 must invert (docs §184).
        let m2 = gray_image(1, 2, 8, &[0, 255], Some(127.5), Some(255.0));
        let mut m1 = m2.clone();
        m1.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME1");
        let a = render_frame(&m2, 0).unwrap();
        let b = render_frame(&m1, 0).unwrap();
        assert_eq!(a.as_bytes()[0], 0, "MONOCHROME2: value 0 → black");
        assert_eq!(b.as_bytes()[0], 255, "MONOCHROME1: value 0 → white (inverted)");
        assert_eq!(a.as_bytes()[4], 255, "MONOCHROME2: value 255 → white");
        assert_eq!(b.as_bytes()[4], 0, "MONOCHROME1: value 255 → black (inverted)");
    }

    #[test]
    fn voilinear_maps_window_endpoints_to_black_and_white() {
        // C.11.2.1.3.1: center 40, width 400 → the full 0..255 range spans ~ -160..240
        let o = GrayscaleRenderOptions::from_window_level(40.0, 400.0);
        // at the window edges the mapping clamps
        assert_eq!(o.map(-160.0), 0, "value at the low window edge is black");
        assert_eq!(o.map(240.0), 255, "value at the high window edge is white");
        // the centre maps to the middle grey
        assert_eq!(o.map(40.0), 128);
    }

    #[test]
    fn width_below_one_falls_back_to_linear_exact_without_panicking() {
        let o = GrayscaleRenderOptions::from_window_level(100.0, 0.5);
        // must not divide by zero; below the centre clamps to black
        assert_eq!(o.map(50.0), 0);
        assert_eq!(o.map(150.0), 255);
    }

    /// Inserts a LUT sequence at `seq_tag` whose one item maps stored values
    /// `[first .. first+entries)` through `data` (8-bit entries).
    fn put_lut_seq(ds: &mut Dataset, seq_tag: Tag, first: i16, data: &[u8]) {
        let seq = ds.insert_sequence(seq_tag);
        let mut item = Dataset::new();
        let entries = data.len() as u16;
        let mut desc = Vec::new();
        desc.extend_from_slice(&entries.to_le_bytes()); // number of entries
        desc.extend_from_slice(&(first as u16).to_le_bytes()); // first mapped (SS)
        desc.extend_from_slice(&8u16.to_le_bytes()); // bits per entry
        item.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::LUT_DESCRIPTOR,
                Vr::Us,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(desc)),
            ),
        ));
        item.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::LUT_DATA,
                Vr::Ow,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(data.to_vec())),
            ),
        ));
        seq.push(item);
    }

    fn put_voi_lut(ds: &mut Dataset, first: i16, data: &[u8]) {
        put_lut_seq(ds, Tag::VOILUT_SEQUENCE, first, data);
    }

    fn put_modality_lut(ds: &mut Dataset, first: i16, data: &[u8]) {
        put_lut_seq(ds, Tag::MODALITY_LUT_SEQUENCE, first, data);
    }

    /// Inserts a 16-bit-entry VOI LUT Sequence mapping `[first ..)` through `data`.
    fn put_voi_lut_16(ds: &mut Dataset, first: i16, data: &[u16]) {
        let seq = ds.insert_sequence(Tag::VOILUT_SEQUENCE);
        let mut item = Dataset::new();
        let mut desc = Vec::new();
        desc.extend_from_slice(&(data.len() as u16).to_le_bytes());
        desc.extend_from_slice(&(first as u16).to_le_bytes());
        desc.extend_from_slice(&16u16.to_le_bytes()); // 16 bits per entry
        item.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::LUT_DESCRIPTOR,
                Vr::Us,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(desc)),
            ),
        ));
        let mut bytes = Vec::new();
        for &v in data {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        item.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::LUT_DATA,
                Vr::Ow,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes)),
            ),
        ));
        seq.push(item);
    }

    #[test]
    fn a_16_bit_voi_lut_is_recalibrated_to_its_own_output_range() {
        // fo-dicom re-windows the LUT through VOILinearLUT(first, last), so a
        // narrow 16-bit table reaches white at its top entry instead of
        // scaling by >>8 (which mapped 1000 to 3)
        let mut ds = gray_image(1, 1, 8, &[5], None, None);
        put_voi_lut_16(&mut ds, 5, &[0, 250, 500, 750, 1000]);
        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert_eq!(o.map(5.0), 0, "first entry is black");
        assert_eq!(o.map(9.0), 255, "top entry is white");
        let mid = o.map(7.0);
        assert!(mid.abs_diff(128) <= 1, "middle entry is mid grey: {mid}");
    }

    #[test]
    fn a_voi_lut_sequence_overrides_the_window_function() {
        // Stored values 10, 11 with a VOI LUT at first_mapped=10 and data
        // [0, 200]: 10 → first entry → 0, 11 → last entry → 255 (the LUT is
        // re-windowed to its own output range, as fo-dicom does).
        let mut ds = gray_image(2, 1, 8, &[10, 11], None, None);
        put_voi_lut(&mut ds, 10, &[0, 200]);

        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert!(o.use_voi_lut && o.voi_lut.is_some(), "VOI LUT detected");
        assert_eq!(o.map(10.0), 0);
        assert_eq!(o.map(11.0), 255);

        let image = render_frame(&ds, 0).unwrap();
        let b = image.as_bytes();
        assert_eq!(b[0], 0, "pixel 0 through the LUT");
        assert_eq!(b[4], 255, "pixel 1 through the LUT");
    }

    #[test]
    fn a_voi_lut_clamps_values_outside_its_range() {
        let mut ds = gray_image(1, 1, 8, &[50], None, None);
        // first_mapped=10, two entries [0, 250]; value 50 is beyond the top
        put_voi_lut(&mut ds, 10, &[0, 250]);
        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert_eq!(o.map(50.0), 255, "above range clamps to the last entry");
        assert_eq!(o.map(0.0), 0, "below range clamps to the first entry");
    }

    #[test]
    fn a_modality_lut_replaces_the_rescale_slope_and_intercept() {
        // stored value 5 with a modality LUT mapping 5 → 200. A rescale
        // slope/intercept is also present and must be ignored; a window centred
        // on 200 then maps the 200 to mid grey.
        let mut ds = gray_image(1, 1, 8, &[5], Some(200.0), Some(400.0));
        ds.insert_str(Tag::RESCALE_SLOPE, Vr::Ds, "2"); // slope (ignored)
        ds.insert_str(Tag::RESCALE_INTERCEPT, Vr::Ds, "0"); // intercept
        put_modality_lut(&mut ds, 5, &[200]);

        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert!(o.modality_lut.is_some(), "modality LUT detected");
        assert_eq!((o.rescale_slope, o.rescale_intercept), (1.0, 0.0), "rescale neutralised");
        // modality LUT: 5 → 200, then window(200,400) → ~128 (mid grey)
        let mapped = o.map(5.0);
        assert!(mapped.abs_diff(128) <= 1, "200 through the window is mid grey: {mapped}");
        // without the modality LUT the raw 5*2=10 would render near black
        assert!(mapped > 100, "the modality LUT, not rescale, drove the value");
    }

    #[test]
    fn a_modality_lut_clamps_and_feeds_the_voi_stage() {
        // storing 8 maps through the modality LUT (first=8, data 200) → 200,
        // which is the VOI LUT's first entry (first=200, data [0, 255]) → black
        let mut ds = gray_image(1, 1, 8, &[8], None, None);
        put_modality_lut(&mut ds, 8, &[200]);
        put_voi_lut(&mut ds, 200, &[0, 255]);
        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert_eq!(o.modality_lut.as_ref().unwrap().map(8.0), 200.0, "modality output");
        assert_eq!(o.map(8.0), 0, "modality output lands on the VOI LUT's first entry");
    }

    #[test]
    fn xa_xrf_display_skips_the_modality_lut() {
        // An XA image with a modality LUT that would map 5→200. Display must
        // ignore it (Clunie guidance) and use the rescale slope/intercept.
        let mut ds = gray_image(1, 1, 8, &[5], Some(10.0), Some(1.0));
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.12.1"); // XA
        ds.insert_str(Tag::RESCALE_SLOPE, Vr::Ds, "3");
        ds.insert_str(Tag::RESCALE_INTERCEPT, Vr::Ds, "0");
        put_modality_lut(&mut ds, 5, &[200]);
        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert!(o.modality_lut.is_none(), "XA display ignores the modality LUT");
        assert_eq!((o.rescale_slope, o.rescale_intercept), (3.0, 0.0), "rescale kept");

        // The same dataset as a CT class does apply the modality LUT.
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.2");
        let ct = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert!(ct.modality_lut.is_some(), "CT applies the modality LUT");
    }

    #[test]
    fn from_histogram_windows_the_central_percent() {
        // 10 distinct values 0..9 (one sample each). A 100% window covers the
        // full range; a tighter percent yields a strictly narrower window.
        let ds = gray_image(10, 1, 8, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], None, None);
        let full = GrayscaleRenderOptions::from_histogram(&ds, 0, 100).unwrap();
        let full_end = full.window_center + full.window_width / 2.0;
        let full_start = full.window_center - full.window_width / 2.0;
        assert!(full_start <= 0.0 && full_end >= 9.0, "100% covers the full range");

        let narrow = GrayscaleRenderOptions::from_histogram(&ds, 0, 50).unwrap();
        assert!(
            narrow.window_width < full.window_width,
            "50% window (w={}) is narrower than 100% (w={})",
            narrow.window_width,
            full.window_width
        );
    }

    #[test]
    fn from_histogram_ignores_a_padding_spike() {
        // 9 padding zeros + the real cluster 100..104; with the padding bin
        // excluded the 100% window is the cluster itself (centre 102, width 4),
        // identical to the same data without the spike. Without trimming the 9
        // zeros would stretch the range to 0..104 (centre 52, width 104).
        let padded = gray_image(14, 1, 8, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 100, 101, 102, 103, 104], None, None);
        let mut padded = padded;
        padded.insert_value(Tag::PIXEL_PADDING_VALUE, 0u16);
        let clean = gray_image(5, 1, 8, &[100, 101, 102, 103, 104], None, None);

        let a = GrayscaleRenderOptions::from_histogram(&padded, 0, 100).unwrap();
        let b = GrayscaleRenderOptions::from_histogram(&clean, 0, 100).unwrap();
        assert_eq!((a.window_center, a.window_width), (102.0, 4.0), "window is the real cluster, not 0..104");
        assert_eq!(
            (a.window_center, a.window_width),
            (b.window_center, b.window_width),
            "padding spike does not change the derived window"
        );
    }

    #[test]
    fn invert_reverses_the_output() {
        let mut o = GrayscaleRenderOptions::from_window_level(40.0, 400.0);
        o.invert = true;
        assert_eq!(o.map(240.0), 0, "white becomes black when inverted");
        assert_eq!(o.map(-160.0), 255);
    }

    #[test]
    fn voilut_function_is_read_from_the_dataset() {
        // (0028,1056) SIGMOID must select the sigmoid window formula.
        let mut ds = gray_image(1, 1, 16, &[0], Some(40.0), Some(400.0));
        ds.insert_str(Tag::VOILUT_FUNCTION, Vr::Cs, "SIGMOID");
        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert_eq!(o.voi_lut_function, VoiFunction::Sigmoid);
        // value 140 = center + 100 → sigmoid ≈ 186 (linear-exact would be 191)
        assert_eq!(o.map(140.0), 186, "sigmoid formula applied from the tag");

        // LINEAR_EXACT is honoured too.
        let mut ds2 = gray_image(1, 1, 16, &[0], Some(40.0), Some(400.0));
        ds2.insert_str(Tag::VOILUT_FUNCTION, Vr::Cs, "LINEAR_EXACT");
        assert_eq!(
            GrayscaleRenderOptions::from_dataset(&ds2).unwrap().voi_lut_function,
            VoiFunction::LinearExact
        );

        // Absent tag → LINEAR.
        let ds3 = gray_image(1, 1, 16, &[0], Some(40.0), Some(400.0));
        assert_eq!(
            GrayscaleRenderOptions::from_dataset(&ds3).unwrap().voi_lut_function,
            VoiFunction::Linear
        );
    }

    #[test]
    fn rescale_is_applied_before_the_window() {
        let mut o = GrayscaleRenderOptions::from_window_level(40.0, 400.0);
        o.rescale_slope = 2.0;
        o.rescale_intercept = 10.0;
        // stored 15 → rescaled 40 → mid grey
        assert_eq!(o.map(15.0), 128);
    }

    #[test]
    fn a_sixteen_bit_frame_renders_to_a_bgra_image() {
        // 2x2 frame, stored values spanning the window
        let samples = [0u16, 255, 512, 1023];
        let ds = gray_image(2, 2, 16, &samples, Some(511.0), Some(1024.0));
        let image = render_grayscale_frame(&ds, 0).unwrap();
        assert_eq!(image.width(), 2);
        assert_eq!(image.height(), 2);
        assert_eq!(image.as_bytes().len(), 2 * 2 * 4);
        // the 4th pixel (largest) should be among the brightest
        let pixels = image.pixels();
        let lum = |p: u32| (p & 0xFF) as u8;
        assert!(lum(pixels[3]) >= lum(pixels[0]), "brighter stored value renders brighter");
    }

    /// Builds a 1-bit MONOCHROME2 frame from packed bytes (8 pixels per byte).
    fn single_bit_image(width: u16, height: u16, packed: &[u8]) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
        ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
        ds.insert_value(Tag::ROWS, height);
        ds.insert_value(Tag::COLUMNS, width);
        ds.insert_value(Tag::BITS_ALLOCATED, 1u16);
        ds.insert_value(Tag::BITS_STORED, 1u16);
        ds.insert_value(Tag::HIGH_BIT, 0u16);
        ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(packed.to_vec())),
            ),
        ));
        ds.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
    }

    fn greys(image: &RawImage) -> Vec<u8> {
        image.as_bytes().as_chunks::<4>().0.iter().map(|p| p[0]).collect()
    }

    #[test]
    fn single_bit_frames_unpack_msb_first_as_black_and_white() {
        // 2x2, one byte per row: 0b1010_0000 → pixels 1,0,1,0 (PS3.3 C.7.6.3.1.1)
        let image = render_frame(&single_bit_image(2, 2, &[0b1010_0000, 0b1010_0000]), 0).unwrap();
        assert_eq!(greys(&image), vec![255, 0, 255, 0], "bit 1 is white, 0 is black");
        // 8x1, byte 0b1000_0000: the first pixel is the most significant bit
        let image = render_frame(&single_bit_image(8, 1, &[0b1000_0000]), 0).unwrap();
        assert_eq!(greys(&image), vec![255, 0, 0, 0, 0, 0, 0, 0]);
    }

    /// Builds a 32-bit MONOCHROME2 frame from raw `u32` sample bits (signed
    /// values are passed as their two's-complement `u32`).
    fn gray_image_32(width: u16, height: u16, signed: bool, samples: &[u32], center: f64, width_ds: f64) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
        ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
        ds.insert_value(Tag::ROWS, height);
        ds.insert_value(Tag::COLUMNS, width);
        ds.insert_value(Tag::BITS_ALLOCATED, 32u16);
        ds.insert_value(Tag::BITS_STORED, 32u16);
        ds.insert_value(Tag::HIGH_BIT, 31u16);
        ds.insert_value(Tag::PIXEL_REPRESENTATION, if signed { 1u16 } else { 0u16 });
        ds.insert_str(Tag::WINDOW_CENTER, Vr::Ds, format!("{center}"));
        ds.insert_str(Tag::WINDOW_WIDTH, Vr::Ds, format!("{width_ds}"));
        let mut bytes = Vec::new();
        for &s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ow,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes)),
            ),
        ));
        ds.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
    }

    #[test]
    fn thirty_two_bit_grayscale_renders_through_the_window() {
        // window centre 1500 / width 3000 over 0,1000,2000,3000 → 0,85,170,255
        let ds = gray_image_32(2, 2, false, &[0, 1000, 2000, 3000], 1500.0, 3000.0);
        let image = render_frame(&ds, 0).unwrap();
        assert_eq!(greys(&image), vec![0, 85, 170, 255]);
    }

    #[test]
    fn signed_thirty_two_bit_grayscale_renders_negative_values() {
        // window centre 0 / width 2000 over -1000,-500,0,1000 → 0,64,128,255
        let samples = [-1000i32, -500, 0, 1000].map(|v| v as u32);
        let ds = gray_image_32(2, 2, true, &samples, 0.0, 2000.0);
        let image = render_frame(&ds, 0).unwrap();
        assert_eq!(greys(&image), vec![0, 64, 128, 255]);
    }

    #[test]
    fn sixty_four_bit_grayscale_is_an_explicit_error() {
        let mut ds = gray_image_32(2, 2, false, &[0, 0, 0, 0], 1.0, 2.0);
        ds.insert_value(Tag::BITS_ALLOCATED, 64u16);
        ds.insert_value(Tag::BITS_STORED, 64u16);
        ds.insert_value(Tag::HIGH_BIT, 63u16);
        let err = render_frame(&ds, 0).unwrap_err();
        assert!(format!("{err}").contains("64-bit"), "error names 64-bit explicitly, got: {err}");

        // also explicit when there is no window tag (the min/max derivation path)
        ds.remove(Tag::WINDOW_CENTER);
        ds.remove(Tag::WINDOW_WIDTH);
        let err = render_frame(&ds, 0).unwrap_err();
        assert!(format!("{err}").contains("64-bit"), "auto-window path also names 64-bit, got: {err}");
    }

    #[test]
    fn parallel_pixel_rows_are_byte_identical_to_serial() {
        // 512x512 = 262144 px exceeds PARALLEL_MIN_PIXELS: threshold 0 takes the
        // threaded path, usize::MAX forces serial. Output must match byte for byte.
        let (w, h) = (512u16, 512u16);
        let samples: Vec<u16> = (0..(w as usize * h as usize)).map(|i| ((i * 37) % 4096) as u16).collect();
        let ds = gray_image(w, h, 16, &samples, Some(2048.0), Some(4096.0));
        let options = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        let serial = render_grayscale_with_threshold(&ds, 0, &options, usize::MAX).unwrap();
        let parallel = render_grayscale_with_threshold(&ds, 0, &options, 0).unwrap();
        assert_eq!(parallel.as_bytes(), serial.as_bytes(), "threaded bands match the serial bytes");
        // the shipped entry point (real threshold) also matches on this large frame
        let shipped = render_grayscale_with_options(&ds, 0, &options).unwrap();
        assert_eq!(shipped.as_bytes(), serial.as_bytes(), "shipped threshold preserves the bytes");
    }

    #[test]
    fn rendered_image_encodes_to_a_well_formed_png() {
        let ds = gray_image(2, 2, 16, &[0, 255, 512, 1023], Some(511.0), Some(1024.0));
        let image = render_grayscale_frame(&ds, 0).unwrap();
        let png = image.to_png();
        // PNG signature
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        // first chunk is IHDR with our dimensions
        assert_eq!(&png[12..16], b"IHDR");
        let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        assert_eq!((width, height), (2, 2));
        // ends with IEND
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn from_dataset_reads_window_and_rescale() {
        let mut ds = gray_image(1, 1, 16, &[100], Some(40.0), Some(400.0));
        ds.insert_str(Tag::RESCALE_INTERCEPT, Vr::Ds, "-1024");
        ds.insert_str(Tag::RESCALE_SLOPE, Vr::Ds, "1");
        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert_eq!(o.window_center, 40.0);
        assert_eq!(o.window_width, 400.0);
        assert_eq!(o.rescale_intercept, -1024.0);
        assert_eq!(o.rescale_slope, 1.0);
    }

    #[test]
    fn a_dataset_without_a_window_falls_back_to_pixel_min_max() {
        // No WindowCenter/Width and no Smallest/Largest tags: priority 4 decodes
        // frame 0 and derives the window from the non-padding min/max (10..30).
        let ds = gray_image(3, 1, 8, &[10, 20, 30], None, None);
        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert_eq!(o.window_center, 20.0);
        assert_eq!(o.window_width, 20.0);
    }

    #[test]
    fn from_min_max_filters_the_pixel_padding_value() {
        // Padding value 200 must be excluded, leaving 10..30 → centre 20, width 20.
        let mut ds = gray_image(4, 1, 8, &[10, 20, 30, 200], None, None);
        ds.insert_value(Tag::PIXEL_PADDING_VALUE, 200u16);
        let o = GrayscaleRenderOptions::from_min_max(&ds, 0).unwrap();
        assert_eq!(o.window_center, 20.0);
        assert_eq!(o.window_width, 20.0);
    }

    #[test]
    fn from_min_max_streams_a_known_sixteen_bit_frame_window() {
        // §3.9: the min/max window is derived without materialising a Vec<f64>.
        // Known samples 7..65535 → centre (7+65535)/2 = 32771, width 65528.
        let samples = [7u16, 100, 4096, 40000, 65535];
        let ds = gray_image(5, 1, 16, &samples, None, None);
        let o = GrayscaleRenderOptions::from_min_max(&ds, 0).unwrap();
        assert_eq!(o.window_center, 32771.0);
        assert_eq!(o.window_width, 65528.0);
    }

    /// Wraps one window pair per frame in a PerFrameFunctionalGroupsSequence,
    /// each item holding a FrameVOILUTSequence (0028,9132) with WC/WW.
    fn put_per_frame_windows(ds: &mut Dataset, windows: &[(f64, f64)]) {
        let per = ds.insert_sequence(crate::model::dataset::PER_FRAME_FUNCTIONAL_GROUPS);
        for &(center, width) in windows {
            let mut fg = Dataset::new();
            {
                let vois = fg.insert_sequence(Tag::FRAME_VOILUT_SEQUENCE);
                let mut item = Dataset::new();
                item.insert_str(Tag::WINDOW_CENTER, Vr::Ds, format!("{center}"));
                item.insert_str(Tag::WINDOW_WIDTH, Vr::Ds, format!("{width}"));
                vois.push(item);
            }
            per.push(fg);
        }
    }

    #[test]
    fn a_per_frame_functional_group_window_is_used() {
        // No top-level WindowCenter/Width; each frame carries its own window.
        let mut ds = gray_image(2, 2, 8, &[10, 20, 30, 40], None, None);
        put_per_frame_windows(&mut ds, &[(100.0, 50.0), (200.0, 20.0)]);

        let pair0 = GrayscaleRenderOptions::from_functional_window_level(&ds, 0).unwrap();
        assert_eq!(pair0, (100.0, 50.0), "frame 0 window from its functional group");
        let pair1 = GrayscaleRenderOptions::from_functional_window_level(&ds, 1).unwrap();
        assert_eq!(pair1, (200.0, 20.0), "frame 1 window from its functional group");

        // priority: from_dataset_frame picks the per-frame window over min/max
        let o = GrayscaleRenderOptions::from_dataset_frame(&ds, 1).unwrap();
        assert_eq!((o.window_center, o.window_width), (200.0, 20.0));
    }

    #[test]
    fn per_frame_functional_window_renders_each_frame_differently() {
        // Two frames with the SAME stored value (100) but each carrying its own
        // window: only a per-frame end-to-end render can honour that (§190).
        let mut ds = gray_image(1, 1, 8, &[100, 100], None, None);
        ds.insert_str(Tag::NUMBER_OF_FRAMES, Vr::Is, "2"); // NumberOfFrames
        put_per_frame_windows(&mut ds, &[(100.0, 2.0), (0.0, 400.0)]);
        let f0 = render_frame(&ds, 0).unwrap();
        let f1 = render_frame(&ds, 1).unwrap();
        assert_eq!(f0.as_bytes()[0], 255, "frame 0 tight window → white");
        assert_eq!(f1.as_bytes()[0], 192, "frame 1 wide window → mid grey");
    }

    #[test]
    fn functional_window_errors_when_absent() {
        // no functional groups at all → error (caller falls back to min/max)
        let ds = gray_image(2, 1, 8, &[10, 20], None, None);
        assert!(GrayscaleRenderOptions::from_functional_window_level(&ds, 0).is_err());
    }

    #[test]
    fn from_bit_range_covers_the_stored_range() {
        // 8-bit unsigned: full range [0, 255] → centre 127.5, width 255.
        let ds = gray_image(1, 1, 8, &[0], None, None);
        let o = GrayscaleRenderOptions::from_bit_range(&ds).unwrap();
        assert_eq!(o.window_center, 127.5);
        assert_eq!(o.window_width, 255.0);

        // 16-bit signed: [-32768, 32767] → centre -0.5, width 65535.
        let mut ds = gray_image(1, 1, 16, &[0], None, None);
        ds.insert_value(Tag::PIXEL_REPRESENTATION, 1u16); // PixelRepresentation signed
        let o = GrayscaleRenderOptions::from_bit_range(&ds).unwrap();
        assert_eq!(o.window_center, -0.5);
        assert_eq!(o.window_width, 65535.0);
    }

    #[test]
    fn from_image_pixel_value_tags_uses_smallest_and_largest() {        let mut ds = gray_image(1, 1, 16, &[0], None, None);
        ds.insert_value(Tag::SMALLEST_IMAGE_PIXEL_VALUE, 0u16); // SmallestImagePixelValue
        ds.insert_value(Tag::LARGEST_IMAGE_PIXEL_VALUE, 1000u16); // LargestImagePixelValue
        let o = GrayscaleRenderOptions::from_image_pixel_value_tags(&ds).unwrap();
        assert_eq!(o.window_center, 500.0);
        assert_eq!(o.window_width, 1000.0);

        // absent tags → error
        let bare = gray_image(1, 1, 8, &[0], None, None);
        assert!(GrayscaleRenderOptions::from_image_pixel_value_tags(&bare).is_err());
    }

    #[test]
    fn grayscale_normalises_high_bit_padding_before_windowing() {
        // 12-bit values stored left-aligned in a 16-bit word (bits_allocated=16,
        // bits_stored=12, high_bit=15). Stored 0x7FF0 must normalise to 2047 and
        // map to mid grey through a [0..4095] window.
        let mut ds = gray_image(1, 1, 16, &[0x7FF0], None, None);
        ds.insert_value(Tag::BITS_STORED, 12u16); // BitsStored
        ds.insert_value(Tag::HIGH_BIT, 15u16); // HighBit
        ds.insert_str(Tag::WINDOW_CENTER, Vr::Ds, "2047.5"); // centre of 12-bit range
        ds.insert_str(Tag::WINDOW_WIDTH, Vr::Ds, "4095"); // full 12-bit width
        let image = render_frame(&ds, 0).unwrap();
        let g = image.as_bytes()[0];
        assert!(g.abs_diff(128) <= 1, "0x7FF0 normalises to 2047 → mid grey: {g}");
    }

    #[test]
    fn crc32_matches_the_known_check_value() {
        // CRC-32 of "123456789" is 0xCBF43926 (the standard check value)
        let mut crc = Crc32::new();
        crc.update(b"123456789");
        assert_eq!(crc.finish(), 0xCBF4_3926);
    }

    fn color_image(pi: &str, samples_per_pixel: u16, bits: u16, samples: &[u16], planar: u16) -> Dataset {
        let count = samples.len() as u16 / samples_per_pixel.max(1);
        let mut ds = Dataset::new();
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, samples_per_pixel);
        ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, pi);
        // square-ish frames for the test sizes used below
        ds.insert_value(Tag::ROWS, 1u16);
        ds.insert_value(Tag::COLUMNS, count);
        ds.insert_value(Tag::BITS_ALLOCATED, bits);
        ds.insert_value(Tag::BITS_STORED, bits);
        ds.insert_value(Tag::HIGH_BIT, bits - 1);
        ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
        if samples_per_pixel > 1 {
            ds.insert_value(Tag::PLANAR_CONFIGURATION, planar);
        }
        let mut bytes = Vec::new();
        for &s in samples {
            if bits == 8 {
                bytes.push(s as u8);
            } else {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
        }
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes)),
            ),
        ));
        ds.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
    }

    #[test]
    fn an_rgb_frame_renders_interleaved_colour() {
        // 2 pixels: red (255,0,0) and green (0,255,0), interleaved RGB, 8-bit
        let ds = color_image("RGB", 3, 8, &[255, 0, 0, 0, 255, 0], 0);
        let image = render_frame(&ds, 0).unwrap();
        assert_eq!(image.width(), 2);
        let b = image.as_bytes();
        // first pixel BGRA = (0,0,255,255) = red
        assert_eq!(&b[0..4], &[0, 0, 255, 255]);
        // second pixel = green (0,255,0)
        assert_eq!(&b[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn a_planar_rgb_frame_renders_the_same_as_interleaved() {
        // same two pixels, planar configuration 1: R plane, G plane, B plane
        let ds = color_image("RGB", 3, 8, &[255, 0, 0, 255, 0, 0], 1);
        let image = render_frame(&ds, 0).unwrap();
        let b = image.as_bytes();
        assert_eq!(&b[0..4], &[0, 0, 255, 255], "pixel 0 is red");
        assert_eq!(&b[4..8], &[0, 255, 0, 255], "pixel 1 is green");
    }

    #[test]
    fn a_ybr_full_frame_converts_to_rgb() {
        // neutral: Y=128, Cb=128, Cr=128 → mid grey
        let ds = color_image("YBR_FULL", 3, 8, &[128, 128, 128], 0);
        let image = render_frame(&ds, 0).unwrap();
        let b = image.as_bytes();
        assert_eq!(&b[0..4], &[128, 128, 128, 0xFF], "neutral YCbCr is grey");

        // pure red in the full-range model: Y=76, Cb=85, Cr=255
        let ds = color_image("YBR_FULL", 3, 8, &[76, 85, 255], 0);
        let image = render_frame(&ds, 0).unwrap();
        let b = image.as_bytes();
        assert!(b[2] > 240 && b[1] < 16 && b[0] < 16, "red-ish BGRA: {:?}", &b[0..4]);
    }

    /// Builds a `width`×`height` 8-bit YBR_FULL frame with explicit stored order.
    fn ybr_full_image(width: u16, height: u16, planar: u16, samples: &[u8]) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, 3u16);
        ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "YBR_FULL");
        ds.insert_value(Tag::ROWS, height);
        ds.insert_value(Tag::COLUMNS, width);
        ds.insert_value(Tag::BITS_ALLOCATED, 8u16);
        ds.insert_value(Tag::BITS_STORED, 8u16);
        ds.insert_value(Tag::HIGH_BIT, 7u16);
        ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
        ds.insert_value(Tag::PLANAR_CONFIGURATION, planar);
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(samples.to_vec())),
            ),
        ));
        ds.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
    }

    #[test]
    fn a_planar_ybr_full_frame_renders_like_the_interleaved_one() {
        // 2x2 pixels p0..p3 = (Y,Cb,Cr): (128,128,128), (100,150,200),
        // (200,100,50), (50,128,128). Planar = the three channel planes.
        let interleaved: &[u8] = &[128, 128, 128, 100, 150, 200, 200, 100, 50, 50, 128, 128];
        let planar: &[u8] = &[128, 100, 200, 50, 128, 150, 100, 128, 128, 200, 50, 128];
        let a = render_frame(&ybr_full_image(2, 2, 0, interleaved), 0).unwrap();
        let b = render_frame(&ybr_full_image(2, 2, 1, planar), 0).unwrap();
        assert_eq!(a.as_bytes(), b.as_bytes(), "planar 1 de-interleaves to the same pixels");
        assert_eq!(&a.as_bytes()[0..4], &[128, 128, 128, 0xFF], "neutral YCbCr is grey");
    }

    /// Builds a 4:2:2 YBR image of `width`×1 with explicit stored bytes.
    fn ybr422_image(pi: &str, width: u16, bytes: &[u8]) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, 3u16);
        ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, pi);
        ds.insert_value(Tag::ROWS, 1u16);
        ds.insert_value(Tag::COLUMNS, width);
        ds.insert_value(Tag::BITS_ALLOCATED, 8u16);
        ds.insert_value(Tag::BITS_STORED, 8u16);
        ds.insert_value(Tag::HIGH_BIT, 7u16);
        ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
        ds.insert_value(Tag::PLANAR_CONFIGURATION, 0u16);
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes.to_vec())),
            ),
        ));
        ds.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
    }

    #[test]
    fn a_planar_ybr_full_422_frame_is_rejected() {
        // PS3.3 / docs §5: YBR_FULL_422 is planar-configuration 0; planar 1 is an
        // error, not a different byte layout to guess at.
        let mut ds = ybr422_image("YBR_FULL_422", 2, &[76, 76, 85, 255]);
        ds.insert_value(Tag::PLANAR_CONFIGURATION, 1u16); // PlanarConfiguration = 1
        let err = render_frame(&ds, 0).unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("planar"), "expected a planar error, got: {err}");
    }

    #[test]
    fn a_ybr_full_422_frame_with_odd_width_pads_the_last_group() {
        // width 3 → ceil(3/2)=2 groups = 8 stored bytes; the last group's Y2 is
        // padding. All three pixels are pure red (Y=76, Cb=85, Cr=255).
        let ds = ybr422_image("YBR_FULL_422", 3, &[76, 76, 85, 255, 76, 0, 85, 255]);
        let image = render_frame(&ds, 0).unwrap();
        assert_eq!(image.width(), 3);
        let b = image.as_bytes();
        for p in 0..3 {
            let px = &b[p * 4..p * 4 + 4];
            assert!(px[2] > 240 && px[1] < 16 && px[0] < 16, "pixel {p} red-ish: {px:?}");
        }
    }

    #[test]
    fn a_ybr_full_422_frame_expands_shared_chroma() {
        // 2 pixels share one CbCr pair: Y1=76, Y2=76, Cb=85, Cr=255 → both red
        let ds = ybr422_image("YBR_FULL_422", 2, &[76, 76, 85, 255]);
        let image = render_frame(&ds, 0).unwrap();
        assert_eq!(image.width(), 2);
        let b = image.as_bytes();
        for p in 0..2 {
            let px = &b[p * 4..p * 4 + 4];
            assert!(px[2] > 240 && px[1] < 16 && px[0] < 16, "pixel {p} red-ish: {px:?}");
        }
    }

    #[test]
    fn a_ybr_partial_422_frame_expands_studio_range() {
        // studio-range neutral: Y=126, Cb=128, Cr=128 → ~mid grey after expansion
        let ds = ybr422_image("YBR_PARTIAL_422", 2, &[126, 126, 128, 128]);
        let image = render_frame(&ds, 0).unwrap();
        let b = image.as_bytes();
        let px = &b[0..4];
        assert!(
            px[0].abs_diff(128) <= 2 && px[1].abs_diff(128) <= 2 && px[2].abs_diff(128) <= 2,
            "partial-range grey expands to ~128: {px:?}"
        );
    }

    #[test]
    fn rgb_low_aligned_stored_bits_are_normalised_before_scaling() {
        // 16-bit RGB carrying 12 significant bits low-aligned (high bit 11):
        // stored 4095 must render as 255, not (4095 >> 8) = 15.
        let mut ds = color_image("RGB", 3, 16, &[4095, 4095, 4095], 0);
        ds.insert_value(Tag::BITS_STORED, 12u16); // BitsStored
        ds.insert_value(Tag::HIGH_BIT, 11u16); // HighBit
        let img = render_frame(&ds, 0).unwrap();
        assert_eq!(&img.as_bytes()[0..4], &[255, 255, 255, 255], "low-aligned 12-bit RGB → white");
    }

    #[test]
    fn sixteen_bit_ybr_full_is_scaled_to_the_eight_bit_range() {
        // y=cb=cr=128<<8 as 16-bit samples must decode to neutral ~grey (128); a
        // missing 16→8 scale treats them as 0..65535 magnitudes and clamps to magenta.
        let ds = color_image("YBR_FULL", 3, 16, &[128 << 8, 128 << 8, 128 << 8], 0);
        let img = render_frame(&ds, 0).unwrap();
        let px = &img.as_bytes()[0..4];
        assert!(
            px[0].abs_diff(128) <= 1 && px[1].abs_diff(128) <= 1 && px[2].abs_diff(128) <= 1,
            "16-bit neutral YBR → ~grey: {px:?}"
        );
    }

    fn us_bytes(values: &[u16]) -> crate::model::buffer::SharedBuffer {
        let mut bytes = Vec::with_capacity(values.len() * 2);
        for &v in values {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes))
    }

    #[test]
    fn a_palette_frame_maps_indices_through_the_colour_lut() {
        let mut ds = color_image("PALETTE COLOR", 1, 8, &[0, 1], 0);
        // 6-byte descriptor: [count=2, first=0, bits=8]
        let desc = || us_bytes(&[2, 0, 8]);
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(Tag::RED_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR, Vr::Us, desc()),
        ));
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(Tag::GREEN_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR, Vr::Us, desc()),
        ));
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(Tag::BLUE_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR, Vr::Us, desc()),
        ));
        // 8-bit data: index 0 → red (255,0,0), index 1 → blue (0,0,255)
        let data = |vals: [u8; 2]| std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vals.to_vec())) as crate::model::buffer::SharedBuffer;
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(Tag::RED_PALETTE_COLOR_LOOKUP_TABLE_DATA, Vr::Ow, data([255, 0])),
        ));
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(Tag::GREEN_PALETTE_COLOR_LOOKUP_TABLE_DATA, Vr::Ow, data([0, 0])),
        ));
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(Tag::BLUE_PALETTE_COLOR_LOOKUP_TABLE_DATA, Vr::Ow, data([0, 255])),
        ));
        let image = render_frame(&ds, 0).unwrap();
        let b = image.as_bytes();
        assert_eq!(&b[0..4], &[0, 0, 255, 255], "index 0 is red");
        assert_eq!(&b[4..8], &[255, 0, 0, 255], "index 1 is blue");
    }

    #[test]
    fn a_palette_frame_honours_the_first_mapped_value() {
        // 2 entries, first mapped 10: stored value v maps to entry (v - 10),
        // out-of-range values clamp to the first/last entry (PS3.3 C.7.6.3.1.5).
        let mut ds = color_image("PALETTE COLOR", 1, 8, &[10, 11, 5, 200], 0);
        for (desc_tag, data_tag, vals) in [
            (0x1101u16, 0x1201u16, [255u8, 0]), // red: entry0=red, entry1=black
            (0x1102, 0x1202, [0u8, 0]),         // green
            (0x1103, 0x1203, [0u8, 255]),       // blue: entry0=black, entry1=blue
        ] {
            ds.insert_or_update(crate::model::item::DicomItem::Element(
                crate::model::element::DicomElement::new_bytes(Tag::new(0x0028, desc_tag), Vr::Us, us_bytes(&[2, 10, 8])),
            ));
            ds.insert_or_update(crate::model::item::DicomItem::Element(
                crate::model::element::DicomElement::new_bytes(
                    Tag::new(0x0028, data_tag),
                    Vr::Ow,
                    std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vals.to_vec())) as crate::model::buffer::SharedBuffer,
                ),
            ));
        }
        let image = render_frame(&ds, 0).unwrap();
        let b = image.as_bytes();
        let px = |i: usize| &b[i * 4..i * 4 + 4];
        assert_eq!(px(0), &[0, 0, 255, 255], "value 10 → entry 0 red");
        assert_eq!(px(1), &[255, 0, 0, 255], "value 11 → entry 1 blue");
        assert_eq!(px(2), &[0, 0, 255, 255], "value 5 (< first) clamps to entry 0 red");
        assert_eq!(px(3), &[255, 0, 0, 255], "value 200 (> last) clamps to entry 1 blue");
    }

    #[test]
    fn scaling_resamples_the_rendered_image() {
        // 2x2 BGRA: red, green / blue, white.
        let img = RawImage::new(
            2,
            2,
            vec![
                0, 0, 255, 255, 0, 255, 0, 255, // row 0: red, green
                255, 0, 0, 255, 255, 255, 255, 255, // row 1: blue, white
            ],
        );
        let up = img.scaled(2.0);
        assert_eq!((up.width(), up.height()), (4, 4), "2x2 scaled by 2 is 4x4");
        assert_eq!(&up.as_bytes()[0..4], &[0, 0, 255, 255], "top-left red preserved");
        let n = up.as_bytes().len();
        assert_eq!(&up.as_bytes()[n - 4..], &[255, 255, 255, 255], "bottom-right white preserved");

        let down = up.scaled(0.5);
        assert_eq!((down.width(), down.height()), (2, 2), "4x4 scaled by 0.5 is 2x2");

        assert_eq!(img.scaled(1.0).as_bytes(), img.as_bytes(), "unit factor is identity");
        assert_eq!(img.scaled(0.0).as_bytes(), img.as_bytes(), "non-positive factor is identity");
    }

    #[test]
    fn scaling_interpolates_in_bgra_order_without_a_colour_round_trip() {
        // black ↔ white along one row: a 3-wide rescale must blend (not pick a
        // nearest neighbour) and keep the BGRA channel order
        let img = RawImage::new(2, 1, vec![0, 0, 0, 255, 250, 250, 250, 255]);
        let up = img.scaled(1.5); // 2 -> 3 columns (and 1 -> 2 rows)
        assert_eq!((up.width(), up.height()), (3, 2));
        let px = |i: usize| {
            let b = up.as_bytes();
            (b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3])
        };
        assert_eq!(px(0), (0, 0, 0, 255), "left edge keeps the dark end");
        assert_eq!(px(2), (250, 250, 250, 255), "right edge keeps the light end");
        let (b, g, r, a) = px(1);
        assert!(b > 0 && b < 250, "the middle pixel is interpolated, got {b}");
        assert_eq!((g, r), (b, b), "grey stays grey, so no channel was swapped");
        assert_eq!(a, 255, "alpha is opaque");
    }

    #[test]
    fn dicom_image_exposes_geometry_and_renders_like_render_frame() {
        let ds = gray_image(3, 2, 16, &[0, 1000, 2000, 3000, 4000, 5000], Some(2500.0), Some(5000.0));
        let img = DicomImage::from_dataset(&ds).unwrap();
        assert_eq!(img.width().unwrap(), 3);
        assert_eq!(img.height().unwrap(), 2);
        assert_eq!(img.number_of_frames().unwrap(), 1);
        assert!(img.is_grayscale().unwrap());
        assert_eq!(img.current_frame(), 0);
        // no overrides → identical to the low-level render
        assert_eq!(img.render().unwrap().as_bytes(), render_frame(&ds, 0).unwrap().as_bytes());
    }

    #[test]
    fn dicom_image_geometry_matches_the_dataset_attributes() {
        // §3.5: the cached accessors must agree with the dataset's own tags.
        let ds = gray_image(3, 2, 16, &[0, 1000, 2000, 3000, 4000, 5000], Some(2500.0), Some(5000.0));
        let img = DicomImage::from_dataset(&ds).unwrap();
        assert_eq!(img.width().unwrap(), ds.get_or(Tag::COLUMNS, 0u32));
        assert_eq!(img.height().unwrap(), ds.get_or(Tag::ROWS, 0u32));
        assert_eq!(img.number_of_frames().unwrap(), 1);
        assert_eq!(img.is_grayscale().unwrap(), ds.get_or(Tag::SAMPLES_PER_PIXEL, 1u16) == 1);
    }

    #[test]
    fn rendering_the_same_dataset_twice_yields_identical_bytes() {
        // §3.4: with the dataset borrowed (cloned per render for now) a second
        // render of the same frame must be byte-identical — no state leaks.
        let ds = gray_image(4, 3, 16, &[0, 100, 500, 1000, 2048, 4000, 5000, 6000, 7000, 8000, 9000, 10000], Some(5000.0), Some(10000.0));
        let first = render_frame(&ds, 0).unwrap();
        let second = render_frame(&ds, 0).unwrap();
        assert_eq!(first.as_bytes(), second.as_bytes());
        let img = DicomImage::from_dataset(&ds).unwrap();
        assert_eq!(img.render().unwrap().as_bytes(), first.as_bytes(), "DicomImage agrees too");
    }

    #[test]
    fn dicom_image_renders_multiple_frames_with_a_voi_lut() {
        // §3.3: the VOI LUT is parsed once per DicomImage; every frame must still
        // render exactly like the free renderer, which re-parses each call.
        let mut ds = gray_image(2, 1, 8, &[10, 11, 20, 21], None, None);
        ds.insert_str(Tag::NUMBER_OF_FRAMES, Vr::Is, "2");
        put_voi_lut(&mut ds, 10, &[0, 255]);
        let img = DicomImage::from_dataset(&ds).unwrap();
        for frame in 0..2u32 {
            assert_eq!(
                img.render_frame(frame).unwrap().as_bytes(),
                render_frame(&ds, frame).unwrap().as_bytes(),
                "frame {frame} matches the re-parsing renderer",
            );
        }
    }

    #[test]
    fn dicom_image_window_override_changes_the_render() {
        // dataset window 2000/4000 → stored 1000 renders ~mid-dark
        let ds = gray_image(1, 1, 16, &[1000], Some(2000.0), Some(4000.0));
        let mut img = DicomImage::from_dataset(&ds).unwrap();
        let default = img.render().unwrap().as_bytes()[0];
        img.set_window(1000.0, 2.0); // tight window centred on the value → bright
        let overridden = img.render().unwrap().as_bytes()[0];
        assert!(overridden > default, "tight override brightens: {overridden} vs {default}");
        img.clear_window();
        assert_eq!(img.render().unwrap().as_bytes()[0], default, "clearing restores the dataset window");
    }

    #[test]
    fn dicom_image_renders_palette_through_the_palette_pipeline() {
        // PALETTE COLOR is single-sample but must NOT go through grayscale
        // windowing (regression: dispatch must key on PI, not samples).
        let mut ds = color_image("PALETTE COLOR", 1, 8, &[0, 1], 0);
        for (desc_tag, data_tag, vals) in [
            (0x1101u16, 0x1201u16, [255u8, 0]),
            (0x1102, 0x1202, [0u8, 0]),
            (0x1103, 0x1203, [0u8, 255]),
        ] {
            ds.insert_or_update(crate::model::item::DicomItem::Element(
                crate::model::element::DicomElement::new_bytes(Tag::new(0x0028, desc_tag), Vr::Us, us_bytes(&[2, 0, 8])),
            ));
            ds.insert_or_update(crate::model::item::DicomItem::Element(
                crate::model::element::DicomElement::new_bytes(
                    Tag::new(0x0028, data_tag),
                    Vr::Ow,
                    std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vals.to_vec())) as crate::model::buffer::SharedBuffer,
                ),
            ));
        }
        let img = DicomImage::from_dataset(&ds).unwrap();
        let a = img.render().unwrap();
        assert_eq!(a.as_bytes(), render_frame(&ds, 0).unwrap().as_bytes(), "palette path matches render_frame");
        assert_eq!(&a.as_bytes()[0..4], &[0, 0, 255, 255], "index 0 red");
        assert_eq!(&a.as_bytes()[4..8], &[255, 0, 0, 255], "index 1 blue");
    }

    #[test]
    fn dicom_image_renders_rgb_through_the_colour_pipeline() {
        let ds = color_image("RGB", 3, 8, &[255, 0, 0, 0, 255, 0], 0);
        let img = DicomImage::from_dataset(&ds).unwrap();
        assert_eq!(img.render().unwrap().as_bytes(), render_frame(&ds, 0).unwrap().as_bytes());
    }

    #[test]
    fn dicom_image_scale_resizes_the_output() {
        let ds = gray_image(2, 2, 8, &[0, 85, 170, 255], Some(128.0), Some(256.0));
        let mut img = DicomImage::from_dataset(&ds).unwrap();
        let base = img.render().unwrap();
        assert_eq!((base.width(), base.height()), (2, 2));
        img.set_scale(2.0);
        let scaled = img.render().unwrap();
        assert_eq!((scaled.width(), scaled.height()), (4, 4), "scale 2 doubles both dimensions");
    }

    #[test]
    fn dicom_image_colormap_tints_the_grayscale_output() {
        let ds = gray_image(2, 1, 8, &[0, 200], Some(128.0), Some(256.0));
        let mut img = DicomImage::from_dataset(&ds).unwrap();
        let base = img.render().unwrap();
        // The ascending (MONOCHROME2) colormap reproduces the plain grey output.
        img.set_colormap(Some(ColorTable::monochrome2()));
        assert_eq!(img.render().unwrap().as_bytes(), base.as_bytes(), "monochrome2 is identity");
        // An all-red colormap paints every pixel red (BGRA [0,0,255,255]).
        img.set_colormap(Some(ColorTable::from_entries([crate::pixel::Color32::new(255, 0, 0); 256])));
        let red = img.render().unwrap();
        for px in red.as_bytes().as_chunks::<4>().0 {
            assert_eq!(&px[..3], &[0, 0, 255], "BGRA red");
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn dicom_image_overlay_toggle_and_colour() {
        // overlay bit only at (0,0); the rest of the 4x4 frame stays grey 128.
        let ds = image_with_embedded_overlay(&[(0, 0)]);
        let mut img = DicomImage::from_dataset(&ds).unwrap();
        // default overlays shown in magenta == render_frame_with_overlays default
        assert_eq!(
            img.render().unwrap().as_bytes(),
            render_frame_with_overlays(&ds, 0, DEFAULT_OVERLAY_COLOR).unwrap().as_bytes(),
        );
        // disabled → identical to the plain render (no overlay pixel)
        img.set_show_overlays(false);
        assert_eq!(img.render().unwrap().as_bytes(), render_frame(&ds, 0).unwrap().as_bytes());
        // custom colour paints the overlay pixel green (BGRA [0,255,0,255])
        img.set_show_overlays(true);
        img.set_overlay_color(crate::pixel::Color32::new(0, 255, 0));
        // OR-composited: green 255 over grey 128 keeps the red/blue channels
        assert_eq!(&img.render().unwrap().as_bytes()[0..4], &[128, 255, 128, 255]);
    }

    #[test]
    fn rgb_png_is_truecolour_with_three_channels() {
        let ds = color_image("RGB", 3, 8, &[255, 0, 0, 0, 255, 0], 0);
        let image = render_frame(&ds, 0).unwrap();
        let png = image.to_png_rgb();
        assert_eq!(&png[12..16], b"IHDR");
        // colour type is byte 9 of IHDR data (offset 16..29)
        assert_eq!(png[16 + 9], 2, "PNG colour type 2 = truecolour");
    }

    #[test]
    fn render_frame_infers_rgb_when_pi_is_absent() {
        // Legacy file: no (0028,0004), 3 samples per pixel → inferred RGB.
        let mut ds = color_image("RGB", 3, 8, &[255, 0, 0, 0, 255, 0], 0);
        ds.remove(Tag::PHOTOMETRIC_INTERPRETATION);
        let img = render_frame(&ds, 0).unwrap();
        assert_eq!(img.width(), 2);
        assert_eq!(&img.as_bytes()[0..4], &[0, 0, 255, 255], "first pixel red");
        assert_eq!(&img.as_bytes()[4..8], &[0, 255, 0, 255], "second pixel green");
    }

    #[test]
    fn bmp_export_has_a_valid_header_and_bottom_up_pixels() {
        // 2x2 BGRA: top row red,green; bottom row blue,white.
        let img = RawImage::new(
            2,
            2,
            vec![
                0, 0, 255, 255, 0, 255, 0, 255, // row 0: red, green
                255, 0, 0, 255, 255, 255, 255, 255, // row 1: blue, white
            ],
        );
        let bmp = img.to_bmp();
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(u32::from_le_bytes([bmp[2], bmp[3], bmp[4], bmp[5]]) as usize, bmp.len(), "file size field");
        assert_eq!(u32::from_le_bytes([bmp[10], bmp[11], bmp[12], bmp[13]]), 54, "pixel-data offset");
        assert_eq!(i32::from_le_bytes([bmp[18], bmp[19], bmp[20], bmp[21]]), 2, "width");
        assert_eq!(i32::from_le_bytes([bmp[22], bmp[23], bmp[24], bmp[25]]), 2, "height");
        assert_eq!(u16::from_le_bytes([bmp[28], bmp[29]]), 24, "24 bits per pixel");
        // 2 px * 3 bytes = 6, padded to 8 per row × 2 rows = 16 + 54 header
        assert_eq!(bmp.len(), 54 + 16);
        // bottom-up: the first stored row is the BOTTOM row (blue, white) as B,G,R.
        assert_eq!(&bmp[54..60], &[255, 0, 0, 255, 255, 255], "bottom row blue,white in BGR");
        // top row (red, green) follows, then 2 padding bytes per row
        assert_eq!(&bmp[62..68], &[0, 0, 255, 0, 255, 0], "top row red,green in BGR");
    }

    #[test]
    fn save_writes_png_or_bmp_by_extension() {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let colour = RawImage::new(1, 1, vec![10, 20, 30, 255]); // not grey

        let png = dir.join(format!("radicom_save_{pid}.png"));
        colour.save(&png).unwrap();
        assert_eq!(&std::fs::read(&png).unwrap()[1..4], b"PNG");
        std::fs::remove_file(&png).ok();

        let bmp = dir.join(format!("radicom_save_{pid}.bmp"));
        colour.save(&bmp).unwrap();
        assert_eq!(&std::fs::read(&bmp).unwrap()[0..2], b"BM");
        std::fs::remove_file(&bmp).ok();

        // a grey pixel saves as a single-channel (colour type 0) PNG
        let grey = RawImage::new(1, 1, vec![128, 128, 128, 255]);
        let gpng = dir.join(format!("radicom_save_grey_{pid}.png"));
        grey.save(&gpng).unwrap();
        let bytes = std::fs::read(&gpng).unwrap();
        assert_eq!(bytes[16 + 9], 0, "grayscale PNG colour type 0");
        std::fs::remove_file(&gpng).ok();
    }

    /// Builds a dataset with a grayscale image plus one embedded overlay group
    /// packed into the pixel-data bits (bit position = bits_stored).
    fn image_with_embedded_overlay(overlay_pixels: &[(u16, u16)]) -> Dataset {
        // 4x4 8-bit MONOCHROME2 image, all mid-grey (128)
        let (w, h) = (4u16, 4u16);
        let mut ds = Dataset::new();
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
        ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
        ds.insert_value(Tag::ROWS, h);
        ds.insert_value(Tag::COLUMNS, w);
        ds.insert_value(Tag::BITS_ALLOCATED, 16u16); // 16-bit so a spare overlay bit exists
        ds.insert_value(Tag::BITS_STORED, 8u16); // 8 bits stored → overlay bit at 8
        ds.insert_value(Tag::HIGH_BIT, 7u16);
        ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
        // window via DS strings (center 128, width 256)
        ds.insert_str(Tag::WINDOW_CENTER, Vr::Ds, "128");
        ds.insert_str(Tag::WINDOW_WIDTH, Vr::Ds, "256");
        // overlay group 0x6000 covering the whole image, bit position 8
        ds.insert_value(Tag::new(0x6000, 0x0010), h); // rows
        ds.insert_value(Tag::new(0x6000, 0x0011), w); // columns
        ds.insert_str(Tag::new(0x6000, 0x0040), Vr::Cs, "G"); // type
        ds.insert_value(Tag::new(0x6000, 0x0100), 1u16); // bits allocated
        ds.insert_value(Tag::new(0x6000, 0x0102), 8u16); // bit position = stored bits
        // OverlayOrigin (6000,0050) is row\column, 1-based: row 1 \ column 1
        let mut origin = Vec::new();
        origin.extend_from_slice(&1i16.to_le_bytes());
        origin.extend_from_slice(&1i16.to_le_bytes());
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::new(0x6000, 0x0050),
                Vr::Ss,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(origin)),
            ),
        ));
        ds.insert_value(Tag::new(0x6000, 0x0051), 1u16); // ImageFrameOrigin: frame 1
        // 16-bit pixel data: base value 128, with overlay bit set at the specified pixels
        let mut bytes = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let mut v = 128u16;
                if overlay_pixels.contains(&(x, y)) {
                    v |= 1 << 8; // overlay bit
                }
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        }
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes)),
            ),
        ));
        ds.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
    }

    #[test]
    fn an_overlay_is_composited_in_the_overlay_colour() {
        // overlay bits at (0,0) and (2,1)
        let ds = image_with_embedded_overlay(&[(0, 0), (2, 1)]);
        let image = render_frame_with_overlays(&ds, 0, DEFAULT_OVERLAY_COLOR).unwrap();
        let b = image.as_bytes();
        // pixel (0,0) → index 0: magenta OR grey 128 → BGRA [255,128,255,255]
        assert_eq!(&b[0..4], &[255, 128, 255, 255], "overlay pixel is OR-composited magenta");
        // pixel (1,0) has no overlay bit → stays grey (128)
        assert_eq!(b[4], 128, "non-overlay pixel keeps its grey value");
        // pixel (2,1) is overlay: row 1, col 2 → index (1*4+2)
        let i = (4 + 2) * 4; // row 1, col 2
        assert_eq!(&b[i..i + 4], &[255, 128, 255, 255], "second overlay pixel is magenta");
    }

    #[test]
    fn an_overlay_origin_outside_the_image_is_clipped_not_panicking() {
        let mut ds = image_with_embedded_overlay(&[]);
        // push the overlay origin far outside the 4x4 image
        ds.insert_value(Tag::new(0x6000, 0x0050), 100i16);
        ds.insert_value(Tag::new(0x6000, 0x0051), 100i16);
        let image = render_frame_with_overlays(&ds, 0, DEFAULT_OVERLAY_COLOR).unwrap();
        assert_eq!(image.width(), 4); // rendered without panic
    }

    #[test]
    fn a_parsed_file_renders_a_real_frame() {
        // End-to-end: a real 256x256 16-bit file must render, not report
        // "frame has zero width or height".
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests-data/explicit-le/GH626.dcm");
        let file = crate::io::file::DicomFile::open(path).expect("GH626 opens");
        let image = render_frame(&file.dataset, 0).expect("frame renders");
        assert_eq!((image.width(), image.height()), (256, 256));
        assert_eq!(image.as_bytes().len(), 256 * 256 * 4, "BGRA bytes for the whole frame");
        assert!(image.as_bytes().as_chunks::<4>().0.iter().any(|px| px[0] != 0), "not a blank image");
    }

    #[test]
    fn ybr_normalises_bits_stored_for_low_aligned_16_bit_data() {
        // 1x1 YBR_FULL, 12 bits stored in 16 (low aligned): 2048 = 128 << 4
        let mut ds = color_image("YBR_FULL", 3, 16, &[2048, 2048, 2048], 0);
        ds.insert_value(Tag::BITS_STORED, 12u16);
        ds.insert_value(Tag::HIGH_BIT, 11u16);
        let image = render_frame(&ds, 0).unwrap();
        assert_eq!(&image.as_bytes()[0..4], &[128, 128, 128, 255], "neutral YCbCr is mid grey");
    }

    #[test]
    fn palette_indices_are_normalised_for_high_aligned_depth() {
        // 12 bits stored, left-aligned in 16: index 3 arrives as 0x3000
        let mut ds = color_image("PALETTE COLOR", 1, 16, &[0x3000], 0);
        ds.insert_value(Tag::BITS_STORED, 12u16);
        ds.insert_value(Tag::HIGH_BIT, 15u16);
        for (desc_tag, data_tag, vals) in [
            (0x1101u16, 0x1201u16, [0u8, 0, 0, 200]),
            (0x1102, 0x1202, [0u8, 0, 0, 100]),
            (0x1103, 0x1203, [0u8, 0, 0, 50]),
        ] {
            ds.insert_or_update(crate::model::item::DicomItem::Element(
                crate::model::element::DicomElement::new_bytes(Tag::new(0x0028, desc_tag), Vr::Us, us_bytes(&[4, 0, 8])),
            ));
            ds.insert_or_update(crate::model::item::DicomItem::Element(
                crate::model::element::DicomElement::new_bytes(
                    Tag::new(0x0028, data_tag),
                    Vr::Ow,
                    std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vals.to_vec()))
                        as crate::model::buffer::SharedBuffer,
                ),
            ));
        }
        let image = render_frame(&ds, 0).unwrap();
        assert_eq!(&image.as_bytes()[0..4], &[50, 100, 200, 255], "0x3000 >> 4 is entry 3");
    }

    #[test]
    fn a_high_aligned_frame_renders_its_own_min_max_window() {
        // 16/12/15: raw 0x0100 and 0x0F00 are stored values 16 and 240; a
        // window derived from the raw bit patterns would render both black.
        let mut ds = gray_image(2, 1, 16, &[0x0100, 0x0F00], None, None);
        ds.insert_value(Tag::BITS_STORED, 12u16);
        ds.insert_value(Tag::HIGH_BIT, 15u16);
        let image = render_frame(&ds, 0).unwrap();
        assert_eq!(image.as_bytes()[0], 0, "minimum is black");
        assert_eq!(image.as_bytes()[4], 255, "maximum is white");
    }

    #[test]
    fn a_window_width_of_one_is_a_threshold() {
        let ds = gray_image(1, 1, 8, &[0], Some(40.0), Some(1.0));
        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert_eq!(o.map(39.0), 0, "below the centre is black");
        assert_eq!(o.map(39.5), 255, "at the centre maps to white (fo-dicom)");
        assert_eq!(o.map(40.0), 255, "above the centre is white");
    }

    #[test]
    fn an_embedded_overlay_is_carved_out_at_its_origin() {
        // 4x4 image; 2x2 overlay with OverlayOrigin row 2 \ column 2, so its
        // bits come from image pixels (1,1) and (2,2)
        let mut ds = gray_image(4, 4, 16, &[128; 16], None, None);
        ds.insert_value(Tag::BITS_STORED, 8u16);
        ds.insert_value(Tag::HIGH_BIT, 7u16);
        ds.insert_value(Tag::new(0x6000, 0x0010), 2u16); // overlay rows
        ds.insert_value(Tag::new(0x6000, 0x0011), 2u16); // overlay columns
        ds.insert_str(Tag::new(0x6000, 0x0040), Vr::Cs, "G");
        ds.insert_value(Tag::new(0x6000, 0x0100), 1u16);
        ds.insert_value(Tag::new(0x6000, 0x0102), 8u16);
        let mut origin = Vec::new();
        origin.extend_from_slice(&2i16.to_le_bytes()); // row
        origin.extend_from_slice(&2i16.to_le_bytes()); // column
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::new(0x6000, 0x0050),
                Vr::Ss,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(origin)),
            ),
        ));
        let mut pixels = Vec::new();
        for y in 0..4u16 {
            for x in 0..4u16 {
                let mut v = 128u16;
                if (x, y) == (1, 1) || (x, y) == (2, 2) {
                    v |= 1 << 8;
                }
                pixels.extend_from_slice(&v.to_le_bytes());
            }
        }
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ow,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(pixels)),
            ),
        ));
        let overlays = crate::pixel::overlay::OverlayData::from_dataset(&ds);
        let data = overlays[0].load(&ds).unwrap();
        assert_eq!(data.data().as_ref(), &[0x01, 0x02], "bits at (1,1) and (2,2) of the image");
    }

    #[test]
    fn the_precalculated_window_matches_per_pixel_mapping() {
        // §3.2: the renderer evaluates the window once per distinct stored value.
        // It must agree exactly with applying the mapping to every pixel, for
        // unsigned and signed data and for an inverted window.
        for (bits, signed, samples) in [
            (8u16, false, vec![0u16, 1, 127, 128, 255, 255, 0, 64]),
            (16u16, false, vec![0u16, 4095, 2048, 1, 4095, 100, 0, 3999]),
            (16u16, true, vec![0x0001u16, 0xFFFF, 0x0F00, 0x8000, 0x7FFF, 5, 0x0100, 0x0FFF]),
        ] {
            let mut ds = gray_image(8, 1, bits, &samples, Some(2000.0), Some(4000.0));
            ds.insert_value(Tag::BITS_STORED, if bits == 8 { 8u16 } else { 12u16 });
            ds.insert_value(Tag::HIGH_BIT, if bits == 8 { 7u16 } else { 11u16 });
            ds.insert_value(Tag::PIXEL_REPRESENTATION, if signed { 1u16 } else { 0u16 });
            let options =
                GrayscaleRenderOptions { invert: true, ..GrayscaleRenderOptions::from_dataset(&ds).unwrap() };
            let image = render_grayscale_with_options(&ds, 0, &options).unwrap();

            let depth = BitDepth::new(bits, options_stored(&ds), options_high_bit(&ds), signed);
            for (i, &raw) in samples.iter().enumerate() {
                let stored = if signed {
                    if bits == 8 { (depth.normalize(raw) as i8) as f64 } else { (depth.normalize(raw) as i16) as f64 }
                } else {
                    depth.normalize(raw) as f64
                };
                let expected = options.map(stored);
                assert_eq!(image.as_bytes()[i * 4], expected, "pixel {i} of {bits}-bit signed={signed}");
            }
        }
    }

    /// `(0028,0102)` of a fixture, defaulting to the allocated width.
    fn options_stored(ds: &Dataset) -> u16 {
        ds.get_or(Tag::BITS_STORED, 16u16)
    }

    /// `(0028,0103)` of a fixture, defaulting to `stored - 1`.
    fn options_high_bit(ds: &Dataset) -> u16 {
        ds.get_or(Tag::HIGH_BIT, options_stored(ds).saturating_sub(1))
    }

    #[test]
    fn a_non_right_angle_rotation_is_ignored() {        let image = RawImage::new(2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]);
        let mut transform = SpatialTransform::new();
        transform.rotate(45);
        let rotated = transform.apply_to(&image);
        assert_eq!(rotated.as_bytes(), image.as_bytes(), "45° is not a right angle");
    }
}
