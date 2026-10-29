//! Icon generation for DICOMDIR records (MEDIA-02; docs/01-architecture.md §12).
//!
//! A DICOMDIR image record may carry an `IconImageSequence` (0088,0200) holding
//! a small (≤128×128), 8-bit MONOCHROME2 preview of the image. This module
//! defines the `IconGenerator` trait and a built-in builder that:
//!
//! 1. selects a frame — the `RepresentativeFrameNumber` (0028,6010) when in
//!    range, else the midpoint frame;
//! 2. downsamples that frame to at most 128×128 by box averaging;
//! 3. builds the icon dataset (8-bit MONOCHROME2, one frame).
//!
//! Only uncompressed 8-/16-bit grayscale is handled; other pixel formats yield
//! `None` rather than a wrong-looking icon (the caller then omits it).

use crate::error::Error;
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::model::vr::Vr;
use crate::pixel::PixelDataRef;

/// The largest icon dimension DICOM allows (§12.2, PS3.3 C.7.6.1.1.6).
pub const MAX_ICON_SIZE: u32 = 128;

/// Builds an icon dataset for a source image, or `None` when unsupported.
pub trait IconGenerator {
    /// The `IconImageSequence` item for `dataset`, if one can be built.
    fn generate(&self, dataset: &Dataset) -> Result<Option<Dataset>, Error>;

    /// The icon for an explicit zero-based source frame.
    ///
    /// Implementors that do not support frame selection retain their existing
    /// behaviour through the default method.
    fn generate_frame(
        &self,
        dataset: &Dataset,
        _frame_index: u32,
    ) -> Result<Option<Dataset>, Error> {
        self.generate(dataset)
    }
}

/// The built-in generator: ≤128×128, 8-bit MONOCHROME2 (§12.2).
#[derive(Debug, Clone, Default)]
pub struct BasicIconGenerator;

impl IconGenerator for BasicIconGenerator {
    fn generate(&self, dataset: &Dataset) -> Result<Option<Dataset>, Error> {
        let Some(icon) = build_icon(dataset, None)? else {
            return Ok(None);
        };
        Ok(Some(icon))
    }

    fn generate_frame(
        &self,
        dataset: &Dataset,
        frame_index: u32,
    ) -> Result<Option<Dataset>, Error> {
        let Some(icon) = build_icon(dataset, Some(frame_index))? else {
            return Ok(None);
        };
        Ok(Some(icon))
    }
}

/// Extracts and downsamples one frame into a MONOCHROME2 icon dataset.
fn build_icon(dataset: &Dataset, requested_frame: Option<u32>) -> Result<Option<Dataset>, Error> {
    let pixel = PixelDataRef::from_dataset(dataset)?;

    // only uncompressed single-sample grayscale is supported here
    if pixel.transfer_syntax().is_encapsulated() || pixel.samples_per_pixel() != 1 {
        return Ok(None);
    }
    let bits = pixel.bits_allocated()?;
    if !matches!(bits, 8 | 16) {
        return Ok(None);
    }
    let (width, height) = (pixel.width(), pixel.height());
    if width == 0 || height == 0 {
        return Ok(None);
    }

    let frames = pixel.number_of_frames().max(1);
    let frame_index =
        requested_frame.unwrap_or_else(|| representative_frame(pixel.dataset(), frames));
    let frame = pixel.get_frame(frame_index)?;
    let raw_data = frame.data()?;
    let raw: &[u8] = raw_data.as_ref();

    // normalise high-bit padding so a 12-bit-in-16 source is not read at 16x
    // magnitude (same rule as the render path; a small local copy because `render`
    // is an optional feature and `media` is always compiled)
    let high_bit = pixel.high_bit();
    let stored = match pixel.bits_stored() {
        0 => bits,
        s => s.min(bits),
    };
    let normalise = |v: u16| -> u16 {
        if stored == 0 || stored > bits || high_bit >= bits {
            return v;
        }
        let up = bits - high_bit - 1;
        let down = bits - stored;
        v.checked_shl(up as u32)
            .unwrap_or(0)
            .wrapping_shr(down as u32)
    };
    // decode samples to 16-bit values in the low `stored` bits
    let samples: Vec<u16> = if bits == 8 {
        raw.iter().map(|&b| normalise(b as u16)).collect()
    } else {
        raw.as_chunks::<2>()
            .0
            .iter()
            .map(|c| normalise(u16::from_le_bytes([c[0], c[1]])))
            .collect()
    };
    let expected = (width as usize) * (height as usize);
    if samples.len() < expected {
        return Ok(None);
    }

    let (icon_w, icon_h) = icon_dimensions(width, height);
    // scale the full stored range (max 255 for 8-bit, 65535 for 16-bit) to 8-bit;
    // clamping (the old behaviour) left every 16-bit source above 255 pure white.
    let scale = 255.0 / ((1u32 << stored) - 1).max(1) as f64;
    let icon_pixels = downsample(&samples, width, height, icon_w, icon_h, scale);

    Ok(Some(icon_dataset(
        width,
        height,
        icon_w,
        icon_h,
        bits,
        &icon_pixels,
    )))
}

/// The frame to preview: RepresentativeFrameNumber in range, else the midpoint.
fn representative_frame(dataset: &Dataset, frame_count: u32) -> u32 {
    // (0028,6010) RepresentativeFrameNumber is US (1-based).
    if let Some(number) = dataset.try_get::<u16>(Tag::REPRESENTATIVE_FRAME_NUMBER) {
        let number = number as u32;
        if number >= 1 && number <= frame_count {
            return number - 1;
        }
    }
    (frame_count - 1) / 2
}

/// Scale preserving aspect ratio so the longer side is at most `MAX_ICON_SIZE`.
fn icon_dimensions(width: u32, height: u32) -> (u32, u32) {
    if width <= MAX_ICON_SIZE && height <= MAX_ICON_SIZE {
        return (width, height);
    }
    let scale = MAX_ICON_SIZE as f64 / width.max(height) as f64;
    let w = ((width as f64 * scale).round() as u32).max(1);
    let h = ((height as f64 * scale).round() as u32).max(1);
    (w, h)
}

/// Box-averages the source into the target size (§12.2, a simple downscale).
fn downsample(
    samples: &[u16],
    width: u32,
    height: u32,
    target_w: u32,
    target_h: u32,
    scale: f64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity((target_w * target_h) as usize);
    for ty in 0..target_h {
        let y0 = (ty as u64 * height as u64 / target_h as u64) as u32;
        let y1 = (((ty + 1) as u64 * height as u64 / target_h as u64) as u32)
            .max(y0 + 1)
            .min(height);
        for tx in 0..target_w {
            let x0 = (tx as u64 * width as u64 / target_w as u64) as u32;
            let x1 = (((tx + 1) as u64 * width as u64 / target_w as u64) as u32)
                .max(x0 + 1)
                .min(width);
            let mut sum = 0u64;
            let mut count = 0u64;
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += samples[(y * width + x) as usize] as u64;
                    count += 1;
                }
            }
            let average = sum as f64 / count.max(1) as f64;
            // scale the stored value range to 8-bit (16-bit sources must not clip)
            out.push((average * scale).round().clamp(0.0, 255.0) as u8);
        }
    }
    out
}

/// Assembles the icon dataset: 8-bit MONOCHROME2, one frame (§12.2).
fn icon_dataset(
    _src_w: u32,
    _src_h: u32,
    icon_w: u32,
    icon_h: u32,
    _src_bits: u16,
    pixels: &[u8],
) -> Dataset {
    let mut icon = Dataset::new();
    icon.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16); // SamplesPerPixel
    icon.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2"); // PhotometricInterpretation
    icon.insert_value(Tag::ROWS, icon_h as u16); // Rows
    icon.insert_value(Tag::COLUMNS, icon_w as u16); // Columns
    icon.insert_value(Tag::BITS_ALLOCATED, 8u16); // BitsAllocated
    icon.insert_value(Tag::BITS_STORED, 8u16); // BitsStored
    icon.insert_value(Tag::HIGH_BIT, 7u16); // HighBit
    icon.insert_value(Tag::PIXEL_REPRESENTATION, 0u16); // PixelRepresentation (unsigned)
    icon.insert_str(Tag::NUMBER_OF_FRAMES, Vr::Is, "1"); // NumberOfFrames (IS)
    icon.insert_or_update(crate::model::item::DicomItem::Element(
        crate::model::element::DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(pixels.to_vec())),
        ),
    ));
    icon
}

/// Attaches an icon as `(0088,0200) IconImageSequence` on a directory record.
pub fn attach_icon(record: &mut Dataset, icon: Dataset) {
    record.insert_sequence(Tag::ICON_IMAGE_SEQUENCE).push(icon);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tag::Tag;

    fn source(
        width: u16,
        height: u16,
        frames: u16,
        bits: u16,
        fill: impl Fn(u32, u32) -> u16,
    ) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
        ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
        ds.insert_value(Tag::ROWS, height);
        ds.insert_value(Tag::COLUMNS, width);
        ds.insert_value(Tag::BITS_ALLOCATED, bits);
        ds.insert_value(Tag::BITS_STORED, bits);
        ds.insert_value(Tag::HIGH_BIT, bits - 1);
        ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
        ds.insert_str(Tag::NUMBER_OF_FRAMES, Vr::Cs, frames.to_string());
        let mut bytes = Vec::new();
        for f in 0..frames {
            for y in 0..height as u32 {
                for x in 0..width as u32 {
                    let v = fill(x, y + f as u32 * height as u32);
                    if bits == 8 {
                        bytes.push(v as u8);
                    } else {
                        bytes.extend_from_slice(&v.to_le_bytes());
                    }
                }
            }
        }
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes)),
            ),
        ));
        ds
    }

    #[test]
    fn a_small_image_becomes_an_eight_bit_icon() {
        let ds = source(64, 32, 1, 8, |x, _| (x * 4) as u16);
        let icon = BasicIconGenerator.generate(&ds).unwrap().unwrap();
        assert_eq!(
            icon.try_get_str(Tag::PHOTOMETRIC_INTERPRETATION).as_deref(),
            Some("MONOCHROME2")
        );
        assert_eq!(icon.try_get::<u16>(Tag::COLUMNS), Some(64), "columns");
        assert_eq!(icon.try_get::<u16>(Tag::ROWS), Some(32), "rows");
        assert_eq!(
            icon.try_get::<u16>(Tag::BITS_ALLOCATED),
            Some(8),
            "8-bit icon"
        );
        // NumberOfFrames (0028,0008) must be IS, not CS (PS3.6)
        assert_eq!(
            icon.get_element(Tag::NUMBER_OF_FRAMES).unwrap().vr(),
            Vr::Is,
            "NumberOfFrames VR is IS"
        );
    }

    #[test]
    fn a_large_image_is_scaled_to_at_most_128() {
        let ds = source(300, 200, 1, 8, |_, _| 7);
        let icon = BasicIconGenerator.generate(&ds).unwrap().unwrap();
        let columns: u32 = icon.try_get::<u16>(Tag::COLUMNS).unwrap() as u32;
        let rows: u32 = icon.try_get::<u16>(Tag::ROWS).unwrap() as u32;
        assert!(
            columns <= 128 && rows <= 128,
            "icon is at most 128x128, got {columns}x{rows}"
        );
        assert_eq!(columns, 128, "the longer side hits the cap");
        assert!(rows < 128, "the shorter side keeps aspect ratio");
    }

    #[test]
    fn a_sixteen_bit_source_is_downconverted_to_eight_bit() {
        let ds = source(16, 16, 1, 16, |_, _| 1000);
        let icon = BasicIconGenerator.generate(&ds).unwrap().unwrap();
        assert_eq!(icon.try_get::<u16>(Tag::BITS_ALLOCATED), Some(8));
        let element = icon.get_element(Tag::PIXEL_DATA).unwrap();
        assert_eq!(element.value().as_buffer().unwrap().size(), 16 * 16);
        // 1000 of 65535 scales to ~4, NOT clipped to 255 (the old white-icon bug)
        let bytes = element.value().as_buffer().unwrap().data().unwrap();
        assert_eq!(
            bytes[0], 4,
            "16-bit value scales to 8-bit, not clipped white"
        );
    }

    #[test]
    fn a_twelve_bit_in_sixteen_source_is_normalised_not_clipped() {
        // 16-bit storage, 12 significant bits left-aligned: stored 0x8000 == 2048.
        let mut ds = source(8, 8, 1, 16, |_, _| 0x8000);
        ds.insert_value(Tag::BITS_STORED, 12u16); // BitsStored; HighBit stays 15
        let icon = BasicIconGenerator.generate(&ds).unwrap().unwrap();
        let bytes = icon
            .get_element(Tag::PIXEL_DATA)
            .unwrap()
            .value()
            .as_buffer()
            .unwrap()
            .data()
            .unwrap();
        assert_eq!(bytes[0], 128, "2048 of 4095 → ~128, not clipped");
    }

    #[test]
    fn representative_frame_number_selects_the_frame() {
        // frame 0 all 10, frame 1 all 20; representative frame 2 (1-based) → 20
        let mut ds = source(4, 4, 2, 8, |_, y| if y < 4 { 10 } else { 20 });
        ds.insert_value(Tag::REPRESENTATIVE_FRAME_NUMBER, 2u16);
        let icon = BasicIconGenerator.generate(&ds).unwrap().unwrap();
        let element = icon.get_element(Tag::PIXEL_DATA).unwrap();
        let bytes = element.value().as_buffer().unwrap().data().unwrap();
        assert_eq!(bytes[0], 20, "the representative frame's value is used");
    }

    #[test]
    fn explicit_frame_index_selects_the_requested_frame() {
        let ds = source(4, 4, 2, 8, |_, y| if y < 4 { 10 } else { 20 });
        let icon = BasicIconGenerator.generate_frame(&ds, 1).unwrap().unwrap();
        let bytes = icon
            .get_element(Tag::PIXEL_DATA)
            .unwrap()
            .value()
            .as_buffer()
            .unwrap()
            .data()
            .unwrap();
        assert_eq!(
            bytes[0], 20,
            "frame index is zero-based and selects frame 1"
        );
    }

    #[test]
    fn a_colour_image_yields_no_icon() {
        let mut ds = source(8, 8, 1, 8, |_, _| 1);
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, 3u16); // RGB
        assert!(BasicIconGenerator.generate(&ds).unwrap().is_none());
    }

    #[test]
    fn an_icon_attaches_as_a_sequence() {
        let mut record = Dataset::new();
        let ds = source(8, 8, 1, 8, |_, _| 5);
        let icon = BasicIconGenerator.generate(&ds).unwrap().unwrap();
        attach_icon(&mut record, icon);
        let sequence = record.try_get_sequence(Tag::ICON_IMAGE_SEQUENCE).unwrap();
        assert_eq!(sequence.items().len(), 1);
    }

    #[test]
    fn empty_geometry_yields_no_icon() {
        let mut ds = source(8, 8, 1, 8, |_, _| 5);
        ds.insert_value(Tag::COLUMNS, 0u16); // Columns = 0
        assert!(BasicIconGenerator.generate(&ds).unwrap().is_none());
    }
}
