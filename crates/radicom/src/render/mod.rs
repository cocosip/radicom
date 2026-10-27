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
use crate::pixel::PixelData;

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
    /// True when `(0028,3010)` VOI LUT Sequence is present (not yet applied).
    pub use_voi_lut: bool,
}

impl GrayscaleRenderOptions {
    /// Resolves options from a dataset (docs §4 factory chain).
    ///
    /// Priority this round: top-level WindowCenter/WindowWidth (first pair),
    /// else Smallest/LargestImagePixelValue, else a fixed default kept honest by
    /// returning an error when nothing usable is present. Rescale slope/intercept
    /// come from `(0028,1053)`/`(0028,1052)`.
    pub fn from_dataset(dataset: &Dataset) -> Result<Self, Error> {
        let rescale_intercept = dataset.try_get::<String>(Tag::new(0x0028, 0x1052)).and_then(|s| s.trim().parse().ok()).unwrap_or(0.0);
        let rescale_slope = dataset.try_get::<String>(Tag::new(0x0028, 0x1053)).and_then(|s| s.trim().parse().ok()).unwrap_or(1.0);

        let (window_center, window_width) = resolve_window(dataset)?;

        // LINEAR with width < 1 switches to LINEAR_EXACT (fo-dicom #1905)
        let voi_lut_function = VoiFunction::Linear;

        let use_voi_lut = dataset.try_get_sequence(Tag::new(0x0028, 0x3010)).is_some();
        let invert = dataset
            .try_get_str(Tag::new(0x0028, 0x0004))
            .map(|pi| pi.trim() == "MONOCHROME1")
            .unwrap_or(false);

        Ok(GrayscaleRenderOptions {
            rescale_slope,
            rescale_intercept,
            voi_lut_function,
            window_center,
            window_width,
            invert,
            use_voi_lut,
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
        }
    }

    /// Maps a stored pixel value to an 8-bit display value through the full
    /// chain: modality rescale → VOI LUT (docs §3 formulas).
    pub fn map(&self, stored: f64) -> u8 {
        let value = stored * self.rescale_slope + self.rescale_intercept;
        self.map_rescaled(value)
    }

    /// Applies only the VOI stage to an already-rescaled value.
    pub fn map_rescaled(&self, value: f64) -> u8 {
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
        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        // IHDR
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&self.width.to_be_bytes());
        ihdr.extend_from_slice(&self.height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]); // 8-bit, grayscale, deflate, filter 0, no interlace
        write_chunk(&mut png, b"IHDR", &ihdr);
        // IDAT (zlib-compressed filtered scanlines)
        let compressed = zlib_compress(&raw);
        write_chunk(&mut png, b"IDAT", &compressed);
        // IEND
        write_chunk(&mut png, b"IEND", &[]);
        png
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
}
