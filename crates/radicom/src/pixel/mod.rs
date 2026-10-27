//! Pixel data facade (PIX-01..03; docs/06-pixel-codec.md §1).
//!
//! `PixelData` is the single read/write gate for `(7FE0,0010)`: codecs, the
//! transcoder and the render module all go through it. It never mutates
//! elements other than the pixel-module tags it exposes as properties.

pub mod photometric;

use crate::error::{Error, ImagingError};
use crate::model::buffer::{
    CompositeBuffer, EmptyBuffer, EndianBuffer, MemoryBuffer, RangeBuffer, SharedBuffer,
};
use crate::model::dataset::Dataset;
use crate::model::item::{DicomFragmentSequence, DicomItem};
use crate::model::tag::Tag;
use crate::model::transfer_syntax::TransferSyntax;
use crate::model::vr::Vr;
use crate::pixel::photometric::PhotometricInterpretation;
use std::sync::Arc;

/// `(7FE0,0010)` Pixel Data.
pub const PIXEL_DATA: Tag = Tag::new(0x7FE0, 0x0010);
/// `(0028,0002)` Samples per Pixel.
pub const SAMPLES_PER_PIXEL: Tag = Tag::new(0x0028, 0x0002);
/// `(0028,0004)` Photometric Interpretation.
pub const PHOTOMETRIC_INTERPRETATION: Tag = Tag::new(0x0028, 0x0004);
/// `(0028,0006)` Planar Configuration.
pub const PLANAR_CONFIGURATION: Tag = Tag::new(0x0028, 0x0006);
/// `(0028,0008)` Number of Frames.
pub const NUMBER_OF_FRAMES: Tag = Tag::new(0x0028, 0x0008);
/// `(0028,0010)` Rows.
pub const ROWS: Tag = Tag::new(0x0028, 0x0010);
/// `(0028,0011)` Columns.
pub const COLUMNS: Tag = Tag::new(0x0028, 0x0011);
/// `(0028,0100)` Bits Allocated.
pub const BITS_ALLOCATED: Tag = Tag::new(0x0028, 0x0100);
/// `(0028,0101)` Bits Stored.
pub const BITS_STORED: Tag = Tag::new(0x0028, 0x0101);
/// `(0028,0102)` High Bit.
pub const HIGH_BIT: Tag = Tag::new(0x0028, 0x0102);
/// `(0028,0103)` Pixel Representation.
pub const PIXEL_REPRESENTATION: Tag = Tag::new(0x0028, 0x0103);
/// `(0028,1101..1103)` / `(0028,1201..1203)` / `(0028,1301..1303)` palette LUTs.
pub const RED_DESCRIPTOR: Tag = Tag::new(0x0028, 0x1101);
pub const GREEN_DESCRIPTOR: Tag = Tag::new(0x0028, 0x1102);
pub const BLUE_DESCRIPTOR: Tag = Tag::new(0x0028, 0x1103);
pub const RED_DATA: Tag = Tag::new(0x0028, 0x1201);
pub const GREEN_DATA: Tag = Tag::new(0x0028, 0x1202);
pub const BLUE_DATA: Tag = Tag::new(0x0028, 0x1203);

/// Storage kind of the pixel data element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelDataKind {
    /// `OB` — 8-bit (or packed 1-bit) native data.
    OtherByte,
    /// `OW` — 16/32/64-bit native data.
    OtherWord,
    /// Fragment sequence (encapsulated syntax).
    Encapsulated,
}

/// An 8-bit-per-channel colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color32 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color32 {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Color32 { r, g, b, a: 255 }
    }
}

/// Read/write gate over a dataset's pixel data.
pub struct PixelData<'a> {
    dataset: &'a mut Dataset,
    kind: PixelDataKind,
}

impl<'a> PixelData<'a> {
    /// Chooses the storage kind for a dataset that is about to receive pixel
    /// data (§1.3).
    pub fn create_new(ds: &'a mut Dataset) -> Result<Self, Error> {
        let syntax = ds.transfer_syntax();
        let bits = ds.get_or(BITS_ALLOCATED, 8u16);
        let kind = if syntax.is_encapsulated() {
            if bits > 16 {
                return Err(pixel_err("encapsulated pixel data supports at most 16 bits allocated"));
            }
            PixelDataKind::Encapsulated
        } else if matches!(bits, 16 | 32 | 64) {
            PixelDataKind::OtherWord
        } else {
            PixelDataKind::OtherByte
        };
        Ok(PixelData { dataset: ds, kind })
    }

    /// Binds to an existing pixel data element, dispatching on its kind.
    pub fn from_dataset(ds: &'a mut Dataset) -> Result<Self, Error> {
        let kind = match ds.item(&PIXEL_DATA) {
            Some(DicomItem::Fragments(_)) => PixelDataKind::Encapsulated,
            Some(DicomItem::Element(e)) => match e.vr() {
                Vr::Ow => PixelDataKind::OtherWord,
                Vr::Ob => PixelDataKind::OtherByte,
                other => {
                    return Err(pixel_err(format!("pixel data has unsupported VR {other}")));
                }
            },
            Some(DicomItem::Sequence(_)) => return Err(pixel_err("pixel data is a sequence")),
            None => {
                // no element yet: pick the kind the syntax implies
                let syntax = ds.transfer_syntax();
                if syntax.is_encapsulated() {
                    PixelDataKind::Encapsulated
                } else if matches!(ds.get_or(BITS_ALLOCATED, 8u16), 16 | 32 | 64) {
                    PixelDataKind::OtherWord
                } else {
                    PixelDataKind::OtherByte
                }
            }
        };
        Ok(PixelData { dataset: ds, kind })
    }

    pub fn kind(&self) -> PixelDataKind {
        self.kind
    }

    pub fn dataset(&self) -> &Dataset {
        self.dataset
    }

    pub fn dataset_mut(&mut self) -> &mut Dataset {
        self.dataset
    }

    // -- properties backed by tags (§1.2) -----------------------------------

    pub fn width(&self) -> u32 {
        self.dataset.get_or(COLUMNS, 0u32)
    }

    pub fn height(&self) -> u32 {
        self.dataset.get_or(ROWS, 0u32)
    }

    /// `(0028,0008)`; absent means one frame.
    pub fn number_of_frames(&self) -> u32 {
        self.dataset.try_get_str(NUMBER_OF_FRAMES).and_then(|s| s.trim().parse().ok()).unwrap_or(1).max(1)
    }

    /// `(0028,0100)`, validated to 1/8/16/32/64.
    pub fn bits_allocated(&self) -> Result<u16, Error> {
        let bits = self.dataset.get_or(BITS_ALLOCATED, 8u16);
        if matches!(bits, 1 | 8 | 16 | 32 | 64) {
            Ok(bits)
        } else {
            Err(pixel_err(format!("unsupported bits allocated {bits}")))
        }
    }

    pub fn bits_stored(&self) -> u16 {
        self.dataset.get_or(BITS_STORED, 0u16)
    }

    pub fn high_bit(&self) -> u16 {
        self.dataset.get_or(HIGH_BIT, 0u16)
    }

    pub fn samples_per_pixel(&self) -> u16 {
        self.dataset.get_or(SAMPLES_PER_PIXEL, 1u16).max(1)
    }

    /// `(0028,0103)`: 1 = signed.
    pub fn pixel_representation(&self) -> u16 {
        self.dataset.get_or(PIXEL_REPRESENTATION, 0u16)
    }

    pub fn planar_configuration(&self) -> u16 {
        self.dataset.get_or(PLANAR_CONFIGURATION, 0u16)
    }

    pub fn photometric_interpretation(&self) -> Option<PhotometricInterpretation> {
        self.dataset.try_get_str(PHOTOMETRIC_INTERPRETATION).map(|s| PhotometricInterpretation::parse(&s))
    }

    pub fn transfer_syntax(&self) -> Arc<TransferSyntax> {
        self.dataset.transfer_syntax()
    }

    // -- frame size (PIX-03, §1.6) ------------------------------------------

    /// Size of one frame once decoded.
    pub fn uncompressed_frame_size(&self) -> Result<u64, Error> {
        let bits = self.bits_allocated()? as u64;
        let samples = self.samples_per_pixel() as u64;
        let mut columns = self.width() as u64;
        let rows = self.height() as u64;
        let pi = self.photometric_interpretation();

        if bits == 1 {
            // bit-packed rows
            return Ok(columns.div_ceil(8) * rows * samples.max(1));
        }
        if pi.is_some_and(|p| p.is_subsampled()) {
            // chroma subsampling: 4:2:2 halves the chroma samples, 4:2:0 both
            columns = columns.div_ceil(2) * 2;
            let plane = columns * rows * bits / 8;
            let chroma = if pi.is_some_and(|p| p.is_420()) {
                plane / 4
            } else {
                plane / 2
            };
            return Ok(plane + 2 * chroma);
        }
        let bytes_per_sample = bits.div_ceil(8);
        Ok(columns * rows * samples * bytes_per_sample)
    }

    // -- frames (PIX-02, §1.4) ----------------------------------------------

    /// One frame's bytes, endianness-normalized (§1.2 guarantee).
    pub fn get_frame(&self, index: u32) -> Result<SharedBuffer, Error> {
        match self.kind {
            PixelDataKind::Encapsulated => self.encapsulated_frame(index),
            PixelDataKind::OtherByte | PixelDataKind::OtherWord => self.native_frame(index),
        }
    }

    /// Native (uncompressed) frame: a zero-copy window over the element.
    fn native_frame(&self, index: u32) -> Result<SharedBuffer, Error> {
        let buffer = match self.dataset.item(&PIXEL_DATA) {
            Some(DicomItem::Element(e)) => e.value().as_buffer().cloned(),
            Some(_) => return Err(pixel_err("pixel data is not a byte value")),
            None => return Err(pixel_err("dataset has no pixel data")),
        }
        .ok_or_else(|| pixel_err("pixel data element holds a string value"))?;

        let frames = self.number_of_frames() as u64;
        let frame_size = self.uncompressed_frame_size()?;
        if index as u64 >= frames {
            return Err(pixel_err(format!("frame {index} out of range (0..{frames})")));
        }
        let offset = index as u64 * frame_size;
        if offset + frame_size > buffer.size() {
            return Err(pixel_err(format!(
                "frame {index} needs bytes {offset}..{} but pixel data holds {}",
                offset + frame_size,
                buffer.size()
            )));
        }
        let window: SharedBuffer = Arc::new(RangeBuffer::new(buffer, offset, frame_size));
        Ok(self.normalize(window))
    }

    /// Encapsulated frame per the §1.4 decision list.
    fn encapsulated_frame(&self, index: u32) -> Result<SharedBuffer, Error> {
        let Some(DicomItem::Fragments(f)) = self.dataset.item(&PIXEL_DATA) else {
            return Err(pixel_err("dataset has no encapsulated pixel data"));
        };
        let fragments = f.fragments();
        // rule 1: a single fragment is the whole data
        if fragments.len() == 1 {
            return Ok(self.normalize(fragments[0].clone()));
        }
        let frames = self.number_of_frames();
        // rule 2: one fragment per frame
        if fragments.len() as u32 == frames {
            let fragment = fragments.get(index as usize).ok_or_else(|| {
                pixel_err(format!("frame {index} out of range (0..{frames})"))
            })?;
            return Ok(self.normalize(fragment.clone()));
        }
        // rule 3: the basic offset table
        if !f.offset_table().is_empty() {
            return self.frame_from_offset_table(f, index, frames);
        }
        // rule 4
        Err(Error::Imaging(ImagingError(
            "encapsulated pixel data has neither an offset table nor one fragment per frame".into(),
        )))
    }

    fn frame_from_offset_table(
        &self,
        f: &DicomFragmentSequence,
        index: u32,
        frames: u32,
    ) -> Result<SharedBuffer, Error> {
        let table = f.offset_table();
        let start = *table.get(index as usize).ok_or_else(|| {
            pixel_err(format!("offset table has no entry for frame {index} (of {frames})"))
        })? as u64;
        let end = table.get(index as usize + 1).map(|v| *v as u64).unwrap_or_else(|| {
            // last frame runs to the end of the fragment stream
            f.fragments().iter().map(|b| b.size() + b.size() % 2).sum::<u64>() + 8 * f.fragments().len() as u64
        });
        if start % 2 != 0 {
            return Err(pixel_err(format!("offset table entry {start} is not even")));
        }
        if end < start {
            return Err(pixel_err(format!("offset table is not monotonic at frame {index}")));
        }

        // Walk the fragments, collecting the byte ranges the frame spans.
        // Offsets are measured from the first fragment's item tag.
        let mut cursor = 0u64;
        let mut parts: Vec<SharedBuffer> = Vec::new();
        for fragment in f.fragments() {
            let item_start = cursor;
            let padded = fragment.size() + fragment.size() % 2;
            let item_end = item_start + 8 + padded;
            if item_end > start && item_start < end {
                let data_start = item_start + 8;
                let local_start = start.saturating_sub(data_start);
                let local_end = (end.min(item_end) - data_start).min(fragment.size());
                if local_end > local_start {
                    parts.push(Arc::new(RangeBuffer::new(
                        fragment.clone(),
                        local_start,
                        local_end - local_start,
                    )));
                }
            }
            cursor = item_end;
        }
        match parts.len() {
            0 => Err(pixel_err(format!("frame {index} spans no fragment bytes"))),
            1 => Ok(self.normalize(parts.pop().expect("len 1"))),
            _ => Ok(self.normalize(Arc::new(CompositeBuffer::new(parts)))),
        }
    }

    /// Appends a frame, returning its index (§1.5).
    pub fn add_frame(&mut self, data: impl AsRef<[u8]>) -> Result<u32, Error> {
        let mut bytes = data.as_ref().to_vec();
        if bytes.len() % 2 == 1 {
            bytes.push(0);
        }
        // the index comes from the frames actually stored, not from
        // `(0028,0008)`, which reads as 1 when absent
        let index = self.stored_frame_count();
        match self.kind {
            PixelDataKind::Encapsulated => {
                let vr = if self.bits_allocated().unwrap_or(8) <= 8 { Vr::Ob } else { Vr::Ow };
                let buffer: SharedBuffer = Arc::new(MemoryBuffer::from(bytes));
                let frags = self.dataset.insert_fragments(PIXEL_DATA, vr);
                frags.push_fragment(buffer);
                self.bump_frame_count(index + 1);
                Ok(index)
            }
            _ => {
                let vr = match self.kind {
                    PixelDataKind::OtherWord => Vr::Ow,
                    _ => Vr::Ob,
                };
                let mut combined = match self.dataset.item(&PIXEL_DATA) {
                    Some(DicomItem::Element(e)) => e.value().as_buffer().map(|b| b.data().to_vec()).unwrap_or_default(),
                    _ => Vec::new(),
                };
                combined.extend_from_slice(&bytes);
                let buffer: SharedBuffer = Arc::new(MemoryBuffer::from(combined));
                self.dataset.insert_or_update(DicomItem::Element(
                    crate::model::element::DicomElement::new_bytes(PIXEL_DATA, vr, buffer),
                ));
                self.bump_frame_count(index + 1);
                Ok(index)
            }
        }
    }

    /// Frames currently present: an absent element holds none, otherwise
    /// `(0028,0008)` (which reads as 1 when the tag is absent) is trusted.
    fn stored_frame_count(&self) -> u32 {
        if self.dataset.contains(&PIXEL_DATA) {
            self.number_of_frames()
        } else {
            0
        }
    }

    /// Sets `(0028,0008)` when there is more than one frame.
    fn bump_frame_count(&mut self, frames: u32) {
        if frames > 1 {
            self.dataset.insert_str(NUMBER_OF_FRAMES, Vr::Is, frames.to_string());
        } else {
            self.dataset.remove(NUMBER_OF_FRAMES);
        }
    }

    /// Replaces the pixel data with an empty element of the current kind.
    pub fn clear(&mut self) {
        match self.kind {
            PixelDataKind::Encapsulated => {
                self.dataset.remove(PIXEL_DATA);
            }
            PixelDataKind::OtherWord => {
                self.dataset
                    .insert_or_update(DicomItem::Element(crate::model::element::DicomElement::new_empty(
                        PIXEL_DATA,
                        Vr::Ow,
                    )));
            }
            PixelDataKind::OtherByte => {
                self.dataset
                    .insert_or_update(DicomItem::Element(crate::model::element::DicomElement::new_empty(
                        PIXEL_DATA,
                        Vr::Ob,
                    )));
            }
        }
        self.dataset.remove(NUMBER_OF_FRAMES);
    }

    /// Wraps a frame so callers always see native byte order (§1.2).
    fn normalize(&self, buffer: SharedBuffer) -> SharedBuffer {
        let unit = match self.bits_allocated() {
            Ok(16) => 2,
            Ok(32) => 4,
            Ok(64) => 8,
            _ => 1,
        };
        let syntax = self.dataset.transfer_syntax();
        let native_big = cfg!(target_endian = "big");
        let swap = unit > 1
            && ((syntax.endian() == crate::model::endian::Endian::Big) != native_big)
            && !syntax.is_encapsulated();
        if swap {
            EndianBuffer::wrap_if_needed(buffer, unit)
        } else {
            buffer
        }
    }

    // -- palette (PIX-04) ---------------------------------------------------

    /// Resolves `PALETTE COLOR` LUTs into 256-entry colour tables (§1.2).
    pub fn palette_color_lut(&self) -> Option<Vec<Color32>> {
        if self.photometric_interpretation() != Some(PhotometricInterpretation::PaletteColor) {
            return None;
        }
        let red = self.read_lut(RED_DESCRIPTOR, RED_DATA)?;
        let green = self.read_lut(GREEN_DESCRIPTOR, GREEN_DATA)?;
        let blue = self.read_lut(BLUE_DESCRIPTOR, BLUE_DATA)?;
        let len = red.len().min(green.len()).min(blue.len());
        Some(
            (0..len)
                .map(|i| Color32::new(red[i], green[i], blue[i]))
                .collect(),
        )
    }

    /// One LUT channel: descriptor-driven 8-bit expansion.
    fn read_lut(&self, descriptor_tag: Tag, data_tag: Tag) -> Option<Vec<u8>> {
        let descriptor = self.dataset.item(&descriptor_tag)?.as_element_buffer()?;
        let desc = descriptor.data();
        if desc.len() < 8 {
            return None;
        }
        let entries = u16::from_le_bytes([desc[2], desc[3]]);
        // a descriptor count of 0 means 65536 entries (PS3.3 C.7.6.3.1.5)
        let count = if entries == 0 { 65536usize } else { entries as usize };
        let first_mapped = u16::from_le_bytes([desc[4], desc[5]]);
        let bits = u16::from_le_bytes([desc[6], desc[7]]);
        let data = self.dataset.item(&data_tag)?.as_element_buffer()?;
        let raw = data.data();

        let mut out = Vec::with_capacity(count);
        match bits {
            8 => {
                for i in 0..count {
                    out.push(*raw.get(i).unwrap_or(&0));
                }
            }
            16 => {
                // 16-bit entries: keep the high byte (fo-dicom truncation rule)
                for i in 0..count {
                    let hi = *raw.get(i * 2 + 1).unwrap_or(&0);
                    out.push(hi);
                }
            }
            _ => return None,
        }
        let _ = first_mapped;
        Some(out)
    }
}


impl DicomItem {
    /// The element's byte buffer, if it holds one.
    fn as_element_buffer(&self) -> Option<&SharedBuffer> {
        match self {
            DicomItem::Element(e) => e.value().as_buffer(),
            _ => None,
        }
    }
}
fn pixel_err(message: impl Into<String>) -> Error {
    Error::Imaging(ImagingError(message.into()))
}

/// Ensures `(7FE0,0010)` exists with a VR matching the kind.
pub fn ensure_pixel_element(ds: &mut Dataset, kind: PixelDataKind) -> Result<(), Error> {
    if ds.contains(&PIXEL_DATA) {
        return Ok(());
    }
    match kind {
        PixelDataKind::Encapsulated | PixelDataKind::OtherByte => {
            ds.insert_or_update(DicomItem::Element(crate::model::element::DicomElement::new_bytes(
                PIXEL_DATA,
                Vr::Ob,
                EmptyBuffer::shared(),
            )));
        }
        PixelDataKind::OtherWord => {
            ds.insert_or_update(DicomItem::Element(crate::model::element::DicomElement::new_bytes(
                PIXEL_DATA,
                Vr::Ow,
                EmptyBuffer::shared(),
            )));
        }
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::transfer_syntax::syntaxes;

    fn mono_dataset(width: u32, height: u32, bits: u16, frames: u32) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(ROWS, height);
        ds.insert_value(COLUMNS, width);
        ds.insert_value(BITS_ALLOCATED, bits);
        ds.insert_value(BITS_STORED, bits);
        ds.insert_value(HIGH_BIT, bits - 1);
        ds.insert_value(SAMPLES_PER_PIXEL, 1u16);
        ds.insert_value(PIXEL_REPRESENTATION, 0u16);
        ds.insert_str(PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
        if frames > 1 {
            ds.insert_str(NUMBER_OF_FRAMES, Vr::Is, frames.to_string());
        }
        ds
    }

    #[test]
    fn create_new_picks_the_storage_kind() {
        let mut ds = mono_dataset(2, 2, 8, 1);
        assert_eq!(PixelData::create_new(&mut ds).unwrap().kind(), PixelDataKind::OtherByte);
        let mut ds = mono_dataset(2, 2, 16, 1);
        assert_eq!(PixelData::create_new(&mut ds).unwrap().kind(), PixelDataKind::OtherWord);
        let mut ds = mono_dataset(2, 2, 16, 1);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        assert_eq!(PixelData::create_new(&mut ds).unwrap().kind(), PixelDataKind::Encapsulated);

        // encapsulated above 16 bits is rejected
        let mut ds = mono_dataset(2, 2, 32, 1);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        assert!(PixelData::create_new(&mut ds).is_err());
    }

    #[test]
    fn frame_size_for_monochrome() {
        let mut ds = mono_dataset(4, 3, 8, 1);
        assert_eq!(PixelData::from_dataset(&mut ds).unwrap().uncompressed_frame_size().unwrap(), 12);
        let mut ds = mono_dataset(4, 3, 16, 1);
        assert_eq!(PixelData::from_dataset(&mut ds).unwrap().uncompressed_frame_size().unwrap(), 24);
        // packed 1-bit rows round up to whole bytes
        let mut ds = mono_dataset(9, 2, 1, 1);
        assert_eq!(PixelData::from_dataset(&mut ds).unwrap().uncompressed_frame_size().unwrap(), 4);
    }

    #[test]
    fn frame_size_for_rgb_and_subsampled_ybr() {
        let mut ds = mono_dataset(4, 4, 8, 1);
        ds.insert_value(SAMPLES_PER_PIXEL, 3u16);
        ds.insert_str(PHOTOMETRIC_INTERPRETATION, Vr::Cs, "RGB");
        assert_eq!(PixelData::from_dataset(&mut ds).unwrap().uncompressed_frame_size().unwrap(), 48);

        ds.insert_str(PHOTOMETRIC_INTERPRETATION, Vr::Cs, "YBR_FULL_422");
        let size = PixelData::from_dataset(&mut ds).unwrap().uncompressed_frame_size().unwrap();
        // 4:2:2: 16 luma + 2 * 8 chroma
        assert_eq!(size, 32);
    }

    #[test]
    fn native_frames_are_zero_copy_windows() {
        let mut ds = mono_dataset(2, 2, 8, 2);
        let bytes = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        ds.insert_or_update(DicomItem::Element(crate::model::element::DicomElement::new_bytes(
            PIXEL_DATA,
            Vr::Ob,
            Arc::new(MemoryBuffer::from(bytes)),
        )));
        let pd = PixelData::from_dataset(&mut ds).unwrap();
        assert_eq!(pd.get_frame(0).unwrap().data().as_ref(), &[1, 2, 3, 4]);
        assert_eq!(pd.get_frame(1).unwrap().data().as_ref(), &[5, 6, 7, 8]);
        assert!(pd.get_frame(2).is_err(), "frame index is range-checked");
    }

    #[test]
    fn short_pixel_data_is_reported() {
        let mut ds = mono_dataset(4, 4, 8, 1);
        ds.insert_or_update(DicomItem::Element(crate::model::element::DicomElement::new_bytes(
            PIXEL_DATA,
            Vr::Ob,
            Arc::new(MemoryBuffer::from(vec![0u8; 4])),
        )));
        let pd = PixelData::from_dataset(&mut ds).unwrap();
        assert!(pd.get_frame(0).is_err(), "declared frame does not fit the buffer");
    }

    #[test]
    fn single_fragment_is_the_whole_data() {
        let mut ds = mono_dataset(2, 2, 8, 3);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        let frags = ds.insert_fragments(PIXEL_DATA, Vr::Ob);
        frags.push_fragment(Arc::new(MemoryBuffer::from(vec![9u8, 9, 9, 9])));
        let pd = PixelData::from_dataset(&mut ds).unwrap();
        // rule 1 wins even though the fragment count does not match frames
        assert_eq!(pd.get_frame(0).unwrap().data().as_ref(), &[9, 9, 9, 9]);
    }

    #[test]
    fn one_fragment_per_frame_maps_directly() {
        let mut ds = mono_dataset(1, 1, 8, 2);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        let frags = ds.insert_fragments(PIXEL_DATA, Vr::Ob);
        frags.push_fragment(Arc::new(MemoryBuffer::from(vec![1u8, 1])));
        frags.push_fragment(Arc::new(MemoryBuffer::from(vec![2u8, 2])));
        let pd = PixelData::from_dataset(&mut ds).unwrap();
        assert_eq!(pd.get_frame(0).unwrap().data().as_ref(), &[1, 1]);
        assert_eq!(pd.get_frame(1).unwrap().data().as_ref(), &[2, 2]);
    }

    #[test]
    fn offset_table_locates_frames_across_fragments() {
        let mut ds = mono_dataset(1, 1, 8, 2);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        let frags = ds.insert_fragments(PIXEL_DATA, Vr::Ob);
        // three fragments, frames are [0..10) and [10..)
        frags.push_fragment(Arc::new(MemoryBuffer::from(vec![1u8, 2])));
        frags.push_fragment(Arc::new(MemoryBuffer::from(vec![3u8, 4])));
        frags.push_fragment(Arc::new(MemoryBuffer::from(vec![5u8, 6])));
        // each item is 8 header bytes + 2 value bytes => offsets 0, 10, 20
        frags.set_offset_table(vec![0, 20]);
        let pd = PixelData::from_dataset(&mut ds).unwrap();
        // frame 0 spans fragments 0 and 1
        assert_eq!(pd.get_frame(0).unwrap().data().as_ref(), &[1, 2, 3, 4], "frame spans fragments");
        assert_eq!(pd.get_frame(1).unwrap().data().as_ref(), &[5, 6]);
    }

    #[test]
    fn missing_offset_table_with_mismatched_fragments_errors() {
        let mut ds = mono_dataset(1, 1, 8, 5);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        let frags = ds.insert_fragments(PIXEL_DATA, Vr::Ob);
        frags.push_fragment(Arc::new(MemoryBuffer::from(vec![1u8, 1])));
        frags.push_fragment(Arc::new(MemoryBuffer::from(vec![2u8, 2])));
        frags.set_offset_table(Vec::new());
        let pd = PixelData::from_dataset(&mut ds).unwrap();
        assert!(pd.get_frame(0).is_err(), "no rule can locate the frame");
    }

    #[test]
    fn add_frame_grows_native_and_encapsulated_data() {
        // 2x2 at 8 bits is 4 bytes per frame
        let mut ds = mono_dataset(2, 2, 8, 1);
        {
            let mut pd = PixelData::from_dataset(&mut ds).unwrap();
            assert_eq!(pd.add_frame([1u8, 2, 3, 4]).unwrap(), 0);
            assert_eq!(pd.add_frame([5u8, 6, 7, 8]).unwrap(), 1);
            assert_eq!(pd.number_of_frames(), 2);
            assert_eq!(pd.get_frame(1).unwrap().data().as_ref(), &[5, 6, 7, 8]);
        }

        let mut ds = mono_dataset(1, 1, 8, 1);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        let mut pd = PixelData::from_dataset(&mut ds).unwrap();
        pd.add_frame([7u8, 8]).unwrap();
        pd.add_frame([9u8, 9]).unwrap();
        assert_eq!(pd.get_frame(1).unwrap().data().as_ref(), &[9, 9]);
    }

    #[test]
    fn palette_lut_expands_8_and_16_bit_entries() {
        let mut ds = mono_dataset(4, 1, 8, 1);
        ds.insert_str(PHOTOMETRIC_INTERPRETATION, Vr::Cs, "PALETTE COLOR");
        // descriptor: 3 entries, first mapped 0, 8 bits
        for (desc_tag, data_tag, values) in [
            (RED_DESCRIPTOR, RED_DATA, [0u8, 100, 200]),
            (GREEN_DESCRIPTOR, GREEN_DATA, [10u8, 110, 210]),
            (BLUE_DESCRIPTOR, BLUE_DATA, [20u8, 120, 220]),
        ] {
            ds.insert_or_update(DicomItem::Element(crate::model::element::DicomElement::new_bytes(
                desc_tag,
                Vr::Us,
                Arc::new(MemoryBuffer::from(vec![0u8, 0, 3, 0, 0, 0, 8, 0])),
            )));
            ds.insert_or_update(DicomItem::Element(crate::model::element::DicomElement::new_bytes(
                data_tag,
                Vr::Ow,
                Arc::new(MemoryBuffer::from(values.to_vec())),
            )));
        }
        let pd = PixelData::from_dataset(&mut ds).unwrap();
        let lut = pd.palette_color_lut().expect("palette LUT");
        assert_eq!(lut.len(), 3);
        assert_eq!(lut[1], Color32::new(100, 110, 120));
    }

    #[test]
    fn palette_lut_is_none_for_other_interpretations() {
        let mut ds = mono_dataset(4, 1, 8, 1);
        assert!(PixelData::from_dataset(&mut ds).unwrap().palette_color_lut().is_none());
    }

    #[test]
    fn unsupported_bits_allocated_is_rejected() {
        let mut ds = mono_dataset(2, 2, 12, 1);
        let pd = PixelData::from_dataset(&mut ds).unwrap();
        assert!(pd.bits_allocated().is_err());
    }
}
