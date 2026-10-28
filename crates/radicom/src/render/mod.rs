//! Grayscale rendering and a zero-dependency PNG writer (REND-01/03/04/07;
//! docs/10-rendering.md §1, §3, §4, §7).
//!
//! This round delivers the smallest vertical slice that satisfies the milestone
//! acceptance test — "render a 16-bit CT frame to PNG correctly":
//! modality rescale → VOI (window) LUT → 8-bit grayscale → `RawImage`
//! (BGRA32, REND-07 default backend) → PNG bytes (zlib via the existing flate2,
//! so no new dependency). Colour/palette pipelines, overlays, spatial
//! transforms and the optional `image` crate backend are later REND work.

use crate::error::{DataError, Error};
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::pixel::overlay::OverlayData;
use crate::pixel::PixelData;

pub mod geometry;
pub mod icon;
pub mod interpolation;
pub mod math;
pub mod reconstruction;
pub mod spatial;

pub use geometry::{
    can_draw_localizer, intersection_localizer, projection_localizer, FrameGeometry, FrameGeometryType,
    FrameOrientation,
};
pub use icon::IconImage;
pub use interpolation::{bilinear_color24, bilinear_grayscale, nearest_grayscale};
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
    entries: Vec<u16>,
    /// Declared bits per entry (8 or 16), used to scale VOI output to a byte.
    bits: u16,
}

impl LutTable {
    /// Builds from a LUT-sequence item's `(0028,3002)` descriptor and
    /// `(0028,3006)` data. Returns `None` when either is missing or malformed.
    pub fn from_item(item: &Dataset) -> Option<LutTable> {
        let desc = element_bytes(item, Tag::new(0x0028, 0x3002))?;
        if desc.len() < 6 {
            return None;
        }
        let entries = u16::from_le_bytes([desc[0], desc[1]]);
        let count = if entries == 0 { 65536usize } else { entries as usize };
        // first mapped pixel value is SS (signed) per PS3.3; keep its bit pattern
        let first_mapped = u16::from_le_bytes([desc[2], desc[3]]) as i16 as i32;
        let bits = u16::from_le_bytes([desc[4], desc[5]]);
        let data = element_bytes(item, Tag::new(0x0028, 0x3006))?;

        let mut out = Vec::with_capacity(count);
        if data.len() == count {
            // 8-bit entries (one byte each) carry the value directly
            out.extend(data[..count].iter().map(|b| *b as u16));
        } else {
            // two bytes per entry (US/OW); 16-bit keeps both, 8-bit-allocated
            // takes the low byte (fo-dicom GetLut semantics)
            for i in 0..count {
                let lo = *data.get(i * 2).unwrap_or(&0) as u16;
                let hi = *data.get(i * 2 + 1).unwrap_or(&0) as u16;
                let v = if bits >= 16 { u16::from_le_bytes([lo as u8, hi as u8]) } else { lo };
                out.push(v);
            }
        }
        Some(LutTable { first_mapped, entries: out, bits })
    }

    /// The entry for `value`, clamped to the table's ends; `None` when empty.
    fn at(&self, value: f64) -> Option<u16> {
        if self.entries.is_empty() {
            return None;
        }
        let index = value.round() as i64 - self.first_mapped as i64;
        let clamped = index.clamp(0, self.entries.len() as i64 - 1) as usize;
        Some(self.entries[clamped])
    }

    /// Maps an input value to an 8-bit display byte (VOI form: 16-bit entries
    /// scale down by 8 to the output range, matching fo-dicom's LUT output stage).
    pub fn map_byte(&self, value: f64) -> u8 {
        match self.at(value) {
            Some(v) if self.bits >= 16 => (v >> 8) as u8,
            Some(v) => v.min(255) as u8,
            None => 0,
        }
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
    /// `use_voi_lut` is set (PS3.3 C.11.2.1.1).
    pub voi_lut: Option<VoiLut>,
    /// Parsed first modality LUT item `(0028,3000)`; when present it replaces the
    /// rescale slope/intercept at the modality stage (docs §4).
    pub modality_lut: Option<ModalityLut>,
}

impl GrayscaleRenderOptions {
    /// Resolves options from a dataset (docs §4 factory chain).
    ///
    /// Priority: a `(0028,3010)` VOI LUT Sequence (else WindowCenter/Width, else
    /// Smallest/LargestImagePixelValue). Rescale comes from a `(0028,3000)`
    /// modality LUT Sequence when present, else `(0028,1053)`/`(0028,1052)`.
    pub fn from_dataset(dataset: &Dataset) -> Result<Self, Error> {
        let rescale_intercept = dataset.try_get::<String>(Tag::new(0x0028, 0x1052)).and_then(|s| s.trim().parse().ok()).unwrap_or(0.0);
        let rescale_slope = dataset.try_get::<String>(Tag::new(0x0028, 0x1053)).and_then(|s| s.trim().parse().ok()).unwrap_or(1.0);

        // A VOI LUT Sequence takes precedence over WindowCenter/Width (docs §4):
        // when its first item is usable, no window is required.
        let voi_lut = dataset
            .try_get_sequence(Tag::new(0x0028, 0x3010))
            .and_then(|seq| seq.items().first())
            .and_then(VoiLut::from_item);
        let (window_center, window_width) = if voi_lut.is_some() {
            (0.0, 0.0)
        } else {
            resolve_window(dataset)?
        };

        // LINEAR with width < 1 switches to LINEAR_EXACT (fo-dicom #1905)
        let voi_lut_function = VoiFunction::Linear;

        let use_voi_lut = voi_lut.is_some();
        let invert = dataset
            .try_get_str(Tag::new(0x0028, 0x0004))
            .map(|pi| pi.trim() == "MONOCHROME1")
            .unwrap_or(false);

        // A modality LUT Sequence (0028,3000) replaces the rescale slope/intercept
        // at the modality stage (docs §4: the sequence LUT takes over).
        let modality_lut = dataset
            .try_get_sequence(Tag::new(0x0028, 0x3000))
            .and_then(|seq| seq.items().first())
            .and_then(ModalityLut::from_item);
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

    /// Options from an explicit window (REND-04 `from_window_level`).
    pub fn from_window_level(center: f64, width: f64) -> Self {
        GrayscaleRenderOptions {
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
                if w < 1.0 {
                    return self.map_rescaled_exact(value);
                }
                ((value - (self.window_center - 0.5)) / (w - 1.0) + 0.5) * 255.0
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

fn linear_exact(value: f64, center: f64, width: f64) -> f64 {
    if width == 0.0 {
        return if value > center { 255.0 } else { 0.0 };
    }
    ((value - center) / width + 0.5) * 255.0
}

/// Resolves the window centre/width (docs §4 priority 1 and 3).
fn resolve_window(dataset: &Dataset) -> Result<(f64, f64), Error> {
    // 1. top-level WindowCenter/WindowWidth, first pair of a possible multi-value
    if let (Some(center), Some(width)) = (
        first_number(dataset, Tag::new(0x0028, 0x1050)),
        first_number(dataset, Tag::new(0x0028, 0x1051)),
    ) {
        return Ok((center, width));
    }
    // 3. Smallest/LargestImagePixelValue → a min/max window
    if let (Some(smallest), Some(largest)) = (
        first_number(dataset, Tag::new(0x0028, 0x0106)),
        first_number(dataset, Tag::new(0x0028, 0x0107)),
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
    let text = dataset.try_get_str(tag)?;
    text.split('\\').next()?.trim().parse::<f64>().ok()
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
            .chunks_exact(4)
            .map(|p| u32::from_le_bytes([p[0], p[1], p[2], p[3]]))
            .collect()
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
                let set = bits[oy * row_bytes + ox / 8] & (0x80 >> (ox % 8)) != 0;
                if !set {
                    continue;
                }
                let px = base_x + ox as isize;
                let py = base_y + oy as isize;
                if px < 0 || py < 0 || px >= self.width as isize || py >= self.height as isize {
                    continue; // clipped
                }
                let i = ((py as u32 * self.width + px as u32) * 4) as usize;
                // BGRA (the render byte layout) — overlay sets the pixel to `color`
                self.bytes[i] = color.b;
                self.bytes[i + 1] = color.g;
                self.bytes[i + 2] = color.r;
                self.bytes[i + 3] = 0xFF;
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
    // PixelData borrows the dataset mutably (frame caches); render from a clone.
    let mut owned = dataset.clone();
    let pixel = PixelData::from_dataset(&mut owned)?;
    if pixel.samples_per_pixel() != 1 {
        return Err(unsupported("only single-sample grayscale can be rendered"));
    }
    let bits = pixel.bits_allocated()?;
    if !matches!(bits, 8 | 16) {
        return Err(unsupported(&format!("bits allocated {bits} is not rendered")));
    }
    if pixel.transfer_syntax().is_encapsulated() {
        return Err(unsupported("compressed frames need the codec layer"));
    }
    let (width, height) = (pixel.width(), pixel.height());
    if width == 0 || height == 0 {
        return Err(unsupported("frame has zero width or height"));
    }

    let options = GrayscaleRenderOptions::from_dataset(dataset)?;
    let buffer = pixel.get_frame(frame)?;
    let raw = buffer.data();
    let raw: &[u8] = raw.as_ref();

    let signed = pixel.pixel_representation() != 0;
    let count = (width as usize) * (height as usize);
    let mut bytes = Vec::with_capacity(count * 4);
    for i in 0..count {
        let stored = if bits == 8 {
            raw.get(i).copied().unwrap_or(0) as f64
        } else {
            let lo = raw.get(i * 2).copied().unwrap_or(0);
            let hi = raw.get(i * 2 + 1).copied().unwrap_or(0);
            let v = u16::from_le_bytes([lo, hi]);
            if signed { (v as i16) as f64 } else { v as f64 }
        };
        let gray = options.map(stored);
        // BGRA with grey in all colour channels, opaque alpha
        bytes.extend_from_slice(&[gray, gray, gray, 0xFF]);
    }
    Ok(RawImage::new(width, height, bytes))
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
    let pi = dataset
        .try_get_str(Tag::new(0x0028, 0x0004))
        .map(|s| Pi::parse(&s))
        .unwrap_or(Pi::Monochrome2);
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
    for overlay in OverlayData::from_dataset(dataset) {
        if !overlay_covers_frame(&overlay, frame) {
            continue;
        }
        if let Err(_error) = image.composite_overlay(&overlay, dataset, color) {
            // a malformed overlay must not break the render (docs §10)
            continue;
        }
    }
    Ok(image)
}

/// True when `overlay` applies to `frame` (docs §1.3 frame-range filtering).
fn overlay_covers_frame(overlay: &OverlayData, frame: u32) -> bool {
    let (frame_origin, _) = overlay.frame_origin();
    let first = frame_origin.max(1) as u32 - 1;
    let count = overlay.number_of_frames().max(1);
    frame >= first && frame < first + count
}

/// Reads `count` native samples for a frame, honouring 8/16-bit depth.
///
/// Returns `(samples, width, height, (samples_per_pixel, planar_configuration))`.
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
        raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
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
    let to8 = |v: u16| -> u8 {
        if bits == 16 { (v >> 8) as u8 } else { v as u8 }
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
    let palette = pixel.palette_color_lut().ok_or_else(|| unsupported("PALETTE COLOR but no palette LUT"))?;
    let (samples, width, height) = read_frame_samples(&pixel, frame)?;
    let count = (width as usize) * (height as usize);
    if samples.len() < count {
        return Err(unsupported("frame is shorter than rows×cols samples"));
    }
    let mut bytes = Vec::with_capacity(count * 4);
    for &index in &samples[..count] {
        let color = palette.get(index as usize).copied().unwrap_or(crate::pixel::Color32::new(0, 0, 0));
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
    let read = |byte_index: usize| -> u16 {
        if bits == 8 {
            raw[byte_index] as u16
        } else {
            u16::from_le_bytes([raw[byte_index], raw[byte_index + 1]])
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
                let y = read(base + (col % 2) * unit) as f64;
                let cb = read(base + 2 * unit) as f64;
                let cr = read(base + 3 * unit) as f64;
                let (r, g, b) = to_rgb(y, cb, cr);
                bytes.extend_from_slice(&[b, g, r, 0xFF]);
            }
        }
    } else {
        // 4:4:4: interleaved Y, Cb, Cr per pixel
        if raw.len() < count * 3 * unit {
            return Err(unsupported("frame is shorter than rows×cols×3 samples"));
        }
        for i in 0..count {
            let base = i * 3 * unit;
            let y = read(base) as f64;
            let cb = read(base + unit) as f64;
            let cr = read(base + 2 * unit) as f64;
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
        ds.insert_value(Tag::new(0x0028, 0x0002), 1u16);
        ds.insert_str(Tag::new(0x0028, 0x0004), Vr::Cs, "MONOCHROME2");
        ds.insert_value(Tag::new(0x0028, 0x0010), height);
        ds.insert_value(Tag::new(0x0028, 0x0011), width);
        ds.insert_value(Tag::new(0x0028, 0x0100), bits);
        ds.insert_value(Tag::new(0x0028, 0x0101), bits);
        ds.insert_value(Tag::new(0x0028, 0x0102), bits - 1);
        ds.insert_value(Tag::new(0x0028, 0x0103), 0u16);
        if let Some(center) = center {
            ds.insert_str(Tag::new(0x0028, 0x1050), Vr::Ds, format!("{center}"));
        }
        if let Some(width_ds) = width_ds {
            ds.insert_str(Tag::new(0x0028, 0x1051), Vr::Ds, format!("{width_ds}"));
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
                Tag::new(0x7FE0, 0x0010),
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes)),
            ),
        ));
        ds.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
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
                Tag::new(0x0028, 0x3002),
                Vr::Us,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(desc)),
            ),
        ));
        item.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::new(0x0028, 0x3006),
                Vr::Ow,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(data.to_vec())),
            ),
        ));
        seq.push(item);
    }

    fn put_voi_lut(ds: &mut Dataset, first: i16, data: &[u8]) {
        put_lut_seq(ds, Tag::new(0x0028, 0x3010), first, data);
    }

    fn put_modality_lut(ds: &mut Dataset, first: i16, data: &[u8]) {
        put_lut_seq(ds, Tag::new(0x0028, 0x3000), first, data);
    }

    /// Inserts a 16-bit-entry VOI LUT Sequence mapping `[first ..)` through `data`.
    fn put_voi_lut_16(ds: &mut Dataset, first: i16, data: &[u16]) {
        let seq = ds.insert_sequence(Tag::new(0x0028, 0x3010));
        let mut item = Dataset::new();
        let mut desc = Vec::new();
        desc.extend_from_slice(&(data.len() as u16).to_le_bytes());
        desc.extend_from_slice(&(first as u16).to_le_bytes());
        desc.extend_from_slice(&16u16.to_le_bytes()); // 16 bits per entry
        item.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::new(0x0028, 0x3002),
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
                Tag::new(0x0028, 0x3006),
                Vr::Ow,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes)),
            ),
        ));
        seq.push(item);
    }

    #[test]
    fn a_16_bit_voi_lut_scales_its_entries_down_to_a_byte() {
        // entry 0x8000 (32768) → output byte 0x80 (128), not clamped to 255
        let mut ds = gray_image(1, 1, 8, &[5], None, None);
        put_voi_lut_16(&mut ds, 5, &[0x8000]);
        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert_eq!(o.map(5.0), 0x80, "16-bit LUT entries scale by >>8");
    }

    #[test]
    fn a_voi_lut_sequence_overrides_the_window_function() {
        // Stored values 10, 11 with a VOI LUT at first_mapped=10 and data
        // [0, 200]: 10 → 0, 11 → 200. No WindowCenter is present.
        let mut ds = gray_image(2, 1, 8, &[10, 11], None, None);
        put_voi_lut(&mut ds, 10, &[0, 200]);

        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert!(o.use_voi_lut && o.voi_lut.is_some(), "VOI LUT detected");
        assert_eq!(o.map(10.0), 0);
        assert_eq!(o.map(11.0), 200);

        let image = render_frame(&ds, 0).unwrap();
        let b = image.as_bytes();
        assert_eq!(b[0], 0, "pixel 0 through the LUT");
        assert_eq!(b[4], 200, "pixel 1 through the LUT");
    }

    #[test]
    fn a_voi_lut_clamps_values_outside_its_range() {
        let mut ds = gray_image(1, 1, 8, &[50], None, None);
        // first_mapped=10, two entries [0, 250]; value 50 is beyond the top
        put_voi_lut(&mut ds, 10, &[0, 250]);
        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert_eq!(o.map(50.0), 250, "above range clamps to the last entry");
        assert_eq!(o.map(0.0), 0, "below range clamps to the first entry");
    }

    #[test]
    fn a_modality_lut_replaces_the_rescale_slope_and_intercept() {
        // stored value 5 with a modality LUT mapping 5 → 200. A rescale
        // slope/intercept is also present and must be ignored; a window centred
        // on 200 then maps the 200 to mid grey.
        let mut ds = gray_image(1, 1, 8, &[5], Some(200.0), Some(400.0));
        ds.insert_str(Tag::new(0x0028, 0x1053), Vr::Ds, "2"); // slope (ignored)
        ds.insert_str(Tag::new(0x0028, 0x1052), Vr::Ds, "0"); // intercept
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
        // storing 8 maps through modality LUT [first=8, data 200] → 200, then a
        // VOI LUT at first=200 with data [42] returns 42.
        let mut ds = gray_image(1, 1, 8, &[8], None, None);
        put_modality_lut(&mut ds, 8, &[200]);
        put_voi_lut(&mut ds, 200, &[42]);
        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert_eq!(o.map(8.0), 42, "modality → VOI chain");
    }

    #[test]
    fn invert_reverses_the_output() {
        let mut o = GrayscaleRenderOptions::from_window_level(40.0, 400.0);
        o.invert = true;
        assert_eq!(o.map(240.0), 0, "white becomes black when inverted");
        assert_eq!(o.map(-160.0), 255);
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
        ds.insert_str(Tag::new(0x0028, 0x1052), Vr::Ds, "-1024");
        ds.insert_str(Tag::new(0x0028, 0x1053), Vr::Ds, "1");
        let o = GrayscaleRenderOptions::from_dataset(&ds).unwrap();
        assert_eq!(o.window_center, 40.0);
        assert_eq!(o.window_width, 400.0);
        assert_eq!(o.rescale_intercept, -1024.0);
        assert_eq!(o.rescale_slope, 1.0);
    }

    #[test]
    fn a_dataset_without_a_window_errors_rather_than_guessing() {
        let ds = gray_image(1, 1, 16, &[100], None, None);
        assert!(GrayscaleRenderOptions::from_dataset(&ds).is_err());
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
        ds.insert_value(Tag::new(0x0028, 0x0002), samples_per_pixel);
        ds.insert_str(Tag::new(0x0028, 0x0004), Vr::Cs, pi);
        // square-ish frames for the test sizes used below
        ds.insert_value(Tag::new(0x0028, 0x0010), 1u16);
        ds.insert_value(Tag::new(0x0028, 0x0011), count);
        ds.insert_value(Tag::new(0x0028, 0x0100), bits);
        ds.insert_value(Tag::new(0x0028, 0x0101), bits);
        ds.insert_value(Tag::new(0x0028, 0x0102), bits - 1);
        ds.insert_value(Tag::new(0x0028, 0x0103), 0u16);
        if samples_per_pixel > 1 {
            ds.insert_value(Tag::new(0x0028, 0x0006), planar);
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
                Tag::new(0x7FE0, 0x0010),
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

    /// Builds a 4:2:2 YBR image of `width`×1 with explicit stored bytes.
    fn ybr422_image(pi: &str, width: u16, bytes: &[u8]) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::new(0x0028, 0x0002), 3u16);
        ds.insert_str(Tag::new(0x0028, 0x0004), Vr::Cs, pi);
        ds.insert_value(Tag::new(0x0028, 0x0010), 1u16);
        ds.insert_value(Tag::new(0x0028, 0x0011), width);
        ds.insert_value(Tag::new(0x0028, 0x0100), 8u16);
        ds.insert_value(Tag::new(0x0028, 0x0101), 8u16);
        ds.insert_value(Tag::new(0x0028, 0x0102), 7u16);
        ds.insert_value(Tag::new(0x0028, 0x0103), 0u16);
        ds.insert_value(Tag::new(0x0028, 0x0006), 0u16);
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::new(0x7FE0, 0x0010),
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes.to_vec())),
            ),
        ));
        ds.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
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
            crate::model::element::DicomElement::new_bytes(Tag::new(0x0028, 0x1101), Vr::Us, desc()),
        ));
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(Tag::new(0x0028, 0x1102), Vr::Us, desc()),
        ));
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(Tag::new(0x0028, 0x1103), Vr::Us, desc()),
        ));
        // 8-bit data: index 0 → red (255,0,0), index 1 → blue (0,0,255)
        let data = |vals: [u8; 2]| std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vals.to_vec())) as crate::model::buffer::SharedBuffer;
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(Tag::new(0x0028, 0x1201), Vr::Ow, data([255, 0])),
        ));
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(Tag::new(0x0028, 0x1202), Vr::Ow, data([0, 0])),
        ));
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(Tag::new(0x0028, 0x1203), Vr::Ow, data([0, 255])),
        ));
        let image = render_frame(&ds, 0).unwrap();
        let b = image.as_bytes();
        assert_eq!(&b[0..4], &[0, 0, 255, 255], "index 0 is red");
        assert_eq!(&b[4..8], &[255, 0, 0, 255], "index 1 is blue");
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

    /// Builds a dataset with a grayscale image plus one embedded overlay group
    /// packed into the pixel-data bits (bit position = bits_stored).
    fn image_with_embedded_overlay(overlay_pixels: &[(u16, u16)]) -> Dataset {
        // 4x4 8-bit MONOCHROME2 image, all mid-grey (128)
        let (w, h) = (4u16, 4u16);
        let mut ds = Dataset::new();
        ds.insert_value(Tag::new(0x0028, 0x0002), 1u16);
        ds.insert_str(Tag::new(0x0028, 0x0004), Vr::Cs, "MONOCHROME2");
        ds.insert_value(Tag::new(0x0028, 0x0010), h);
        ds.insert_value(Tag::new(0x0028, 0x0011), w);
        ds.insert_value(Tag::new(0x0028, 0x0100), 16u16); // 16-bit so a spare overlay bit exists
        ds.insert_value(Tag::new(0x0028, 0x0101), 8u16); // 8 bits stored → overlay bit at 8
        ds.insert_value(Tag::new(0x0028, 0x0102), 7u16);
        ds.insert_value(Tag::new(0x0028, 0x0103), 0u16);
        // window via DS strings (center 128, width 256)
        ds.insert_str(Tag::new(0x0028, 0x1050), Vr::Ds, "128");
        ds.insert_str(Tag::new(0x0028, 0x1051), Vr::Ds, "256");
        // overlay group 0x6000 covering the whole image, bit position 8
        ds.insert_value(Tag::new(0x6000, 0x0010), h); // rows
        ds.insert_value(Tag::new(0x6000, 0x0011), w); // columns
        ds.insert_str(Tag::new(0x6000, 0x0040), Vr::Cs, "G"); // type
        ds.insert_value(Tag::new(0x6000, 0x0100), 1u16); // bits allocated
        ds.insert_value(Tag::new(0x6000, 0x0102), 8u16); // bit position = stored bits
        ds.insert_value(Tag::new(0x6000, 0x0051), 1i16); // origin y = 1
        ds.insert_value(Tag::new(0x6000, 0x0050), 1i16); // origin x = 1
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
                Tag::new(0x7FE0, 0x0010),
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
        // pixel (0,0) → index 0: magenta (r=255,g=0,b=255) in BGRA
        assert_eq!(&b[0..4], &[255, 0, 255, 255], "overlay pixel is magenta");
        // pixel (1,0) has no overlay bit → stays grey (128)
        assert_eq!(b[4], 128, "non-overlay pixel keeps its grey value");
        // pixel (2,1) is overlay: row 1, col 2 → index (1*4+2)
        let i = (4 + 2) * 4; // row 1, col 2
        assert_eq!(&b[i..i + 4], &[255, 0, 255, 255], "second overlay pixel is magenta");
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
}
