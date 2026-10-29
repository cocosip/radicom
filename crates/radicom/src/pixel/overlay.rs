//! Overlay data access (PIX-05; docs/01-architecture.md §9).

use crate::error::{Error, ImagingError};
use crate::model::buffer::{MemoryBuffer, SharedBuffer};
use crate::model::dataset::Dataset;
use crate::model::item::DicomItem;
use crate::model::tag::Tag;
use crate::model::vr::Vr;
use std::sync::Arc;

/// Candidate overlay groups are `0x6000..=0x60FF`, even.
const OVERLAY_GROUPS: std::ops::RangeInclusive<u16> = 0x6000..=0x60FF;

/// One overlay plane bound to a repeating group `60xx`.
#[derive(Debug, Clone)]
pub struct OverlayData {
    /// The repeating group, e.g. `0x6000`.
    pub group: u16,
    /// `(60xx,3000)` when present.
    pub data: Option<SharedBuffer>,
    rows: u16,
    columns: u16,
    /// `(60xx,0050)` OverlayOrigin, row\column, re-ordered to `(x, y)`.
    origin: (i16, i16),
    /// `(60xx,0051)` ImageFrameOrigin, 1-based frame number.
    frame_origin: i16,
    number_of_frames: u32,
    bits_allocated: u16,
    bit_position: u16,
    overlay_type: String,
    label: Option<String>,
    description: Option<String>,
}

impl OverlayData {
    /// Every qualifying overlay group in the dataset (§1.7).
    pub fn from_dataset(ds: &Dataset) -> Vec<OverlayData> {
        let mut out = Vec::new();
        let mut group = *OVERLAY_GROUPS.start();
        while group <= *OVERLAY_GROUPS.end() {
            // a group qualifies when (gggg,0010) exists with a non-zero shape
            if ds.contains(&Tag::new(group, 0x0010)) {
                if let Some(overlay) = Self::read(ds, group) {
                    out.push(overlay);
                }
            }
            group += 2;
        }
        out
    }

    fn read(ds: &Dataset, group: u16) -> Option<OverlayData> {
        let columns = ds.get_or(Tag::new(group, 0x0011), 0u16);
        let rows = ds.get_or(Tag::new(group, 0x0010), 0u16);
        let overlay_type = ds
            .try_get_str(Tag::new(group, 0x0040))
            .unwrap_or_default()
            .trim()
            .to_string();
        if rows == 0 || columns == 0 || overlay_type.is_empty() {
            return None;
        }
        let data = match ds.item(&Tag::new(group, 0x3000)) {
            Some(DicomItem::Element(e)) => e.value().as_buffer().cloned(),
            _ => None,
        };
        let origin_tag = Tag::new(group, 0x0050);
        // PS3.3 C.9.2: OverlayOrigin is row\column (VM 2); expose (x, y).
        let row = ds.get_at::<i16>(&origin_tag, 0).unwrap_or(0);
        let column = ds.get_at::<i16>(&origin_tag, 1).unwrap_or(0);
        Some(OverlayData {
            group,
            data,
            rows,
            columns,
            origin: (column, row),
            // PS3.3 C.9.2.1.4: ImageFrameOrigin (60xx,0051), US VM 1
            frame_origin: ds.get_or(Tag::new(group, 0x0051), 0u16) as i16,
            number_of_frames: ds
                .try_get_str(Tag::new(group, 0x0015))
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(1)
                .max(1),
            bits_allocated: ds.get_or(Tag::new(group, 0x0100), 1u16),
            bit_position: ds.get_or(Tag::new(group, 0x0102), 0u16),
            overlay_type,
            label: ds
                .try_get_str(Tag::new(group, 0x1500))
                .map(|s| s.trim().to_string()),
            description: ds
                .try_get_str(Tag::new(group, 0x0022))
                .map(|s| s.trim().to_string()),
        })
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn columns(&self) -> u16 {
        self.columns
    }

    pub fn origin(&self) -> (i16, i16) {
        self.origin
    }

    pub fn frame_origin(&self) -> i16 {
        self.frame_origin
    }

    pub fn number_of_frames(&self) -> u32 {
        self.number_of_frames
    }

    pub fn bits_allocated(&self) -> u16 {
        self.bits_allocated
    }

    pub fn bit_position(&self) -> u16 {
        self.bit_position
    }

    pub fn overlay_type(&self) -> &str {
        &self.overlay_type
    }

    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Resolves the overlay bytes, extracting an embedded bit plane when
    /// `(60xx,3000)` is absent (§1.7 rules 1 and 2). Embedded planes are taken
    /// from frame 0; use [`OverlayData::load_frame`] for multi-frame images.
    pub fn load(&self, ds: &Dataset) -> Result<SharedBuffer, Error> {
        self.load_frame(ds, 0)
    }

    /// Resolves the overlay bytes for `frame`. A standalone `(60xx,3000)`
    /// element is returned as-is; an embedded plane is carved out of that
    /// image frame.
    pub fn load_frame(&self, ds: &Dataset, frame: u32) -> Result<SharedBuffer, Error> {
        if let Some(data) = &self.data {
            return Ok(data.clone());
        }
        Self::extract_embedded_frame(ds, self.group, frame)
    }

    /// Extracts the embedded bit plane of `group` from frame 0 into an
    /// unpacked 1-bit buffer.
    pub fn extract_embedded(ds: &Dataset, group: u16) -> Result<SharedBuffer, Error> {
        Self::extract_embedded_frame(ds, group, 0)
    }

    /// Extracts the embedded bit plane of `group` from `frame` into an
    /// unpacked 1-bit buffer.
    pub fn extract_embedded_frame(
        ds: &Dataset,
        group: u16,
        frame: u32,
    ) -> Result<SharedBuffer, Error> {
        let syntax = ds.transfer_syntax();
        if syntax.is_encapsulated() {
            return Err(Error::Imaging(ImagingError(
                "embedded overlays require a decompressed dataset".into(),
            )));
        }
        let bit_position = ds.get_or(Tag::new(group, 0x0102), 0u16);
        let rows = ds.get_or(Tag::new(group, 0x0010), 0u16) as u64;
        let columns = ds.get_or(Tag::new(group, 0x0011), 0u16) as u64;
        if rows == 0 || columns == 0 {
            return Err(Error::Imaging(ImagingError(
                "embedded overlay has no shape".into(),
            )));
        }

        let pixel =
            match ds.item(&crate::pixel::PIXEL_DATA) {
                Some(DicomItem::Element(e)) => e.value().as_buffer().cloned().ok_or_else(|| {
                    Error::Imaging(ImagingError("pixel data holds no bytes".into()))
                })?,
                _ => {
                    return Err(Error::Imaging(ImagingError(
                        "dataset has no pixel data".into(),
                    )))
                }
            };
        let bits = ds.get_or(crate::pixel::BITS_ALLOCATED, 8u16);
        let bits_stored = ds.get_or(crate::pixel::BITS_STORED, bits);
        // absent HighBit implies BitsStored - 1 (PS3.3 C.7.6.3.1.2)
        let high_bit = ds.get_or(crate::pixel::HIGH_BIT, bits_stored.saturating_sub(1));
        // fo-dicom DicomOverlayData.Load (#110): the bit position lies inside
        // the stored pixel bits, so it is real pixel data, not an overlay.
        // The test is expressed against HighBit, not BitsStored, so it is
        // correct for high-aligned depth metadata too.
        if bit_position <= high_bit && bit_position > high_bit.saturating_sub(bits_stored) {
            return Err(Error::Imaging(ImagingError(format!(
                "overlay bit position {bit_position} lies inside the stored pixel bits"
            ))));
        }
        if !matches!(bits, 8 | 16) {
            return Err(Error::Imaging(ImagingError(format!(
                "embedded overlays support 8- and 16-bit pixel data, not {bits}"
            ))));
        }

        // The overlay plane is carved out of the image frame starting at
        // OverlayOrigin (1-based row\column), exactly like fo-dicom.
        let image_columns = ds.get_or(crate::pixel::COLUMNS, 0u16) as usize;
        let image_rows = ds.get_or(crate::pixel::ROWS, 0u16) as usize;
        if image_columns == 0 || image_rows == 0 {
            return Err(Error::Imaging(ImagingError(
                "dataset has no image dimensions".into(),
            )));
        }
        let origin_tag = Tag::new(group, 0x0050);
        let row = ds.get_at::<i16>(&origin_tag, 0).unwrap_or(0);
        let column = ds.get_at::<i16>(&origin_tag, 1).unwrap_or(0);
        let ox = (column as isize - 1).max(0) as usize;
        let oy = (row as isize - 1).max(0) as usize;

        let raw = pixel.data()?;
        let unit = (bits / 8) as usize;
        let frame_size = image_columns * image_rows * unit;
        let start = frame as usize * frame_size;
        if raw.len() < start + frame_size {
            return Err(Error::Imaging(ImagingError(format!(
                "frame {frame} is shorter than the image shape"
            ))));
        }
        let frame_bytes = &raw[start..start + frame_size];
        // 1 bit per pixel, row-packed, LSB first (PS3.5 §8.1.2)
        let row_bytes = columns.div_ceil(8) as usize;
        let mut out = vec![0u8; row_bytes * rows as usize];
        let mut o = 0usize;
        for y in 0..rows as usize {
            for x in 0..columns as usize {
                let (ix, iy) = (ox + x, oy + y);
                if ix >= image_columns || iy >= image_rows {
                    continue; // outside the image: no overlay bit
                }
                let idx = (iy * image_columns + ix) * unit;
                let value = if unit == 1 {
                    frame_bytes[idx] as u16
                } else {
                    // in-memory pixel buffers are host byte order (§1.2)
                    u16::from_ne_bytes([frame_bytes[idx], frame_bytes[idx + 1]])
                };
                if (value >> bit_position) & 1 == 1 {
                    out[o + x / 8] |= 0x01 << (x % 8);
                }
            }
            o += row_bytes;
        }
        Ok(Arc::new(MemoryBuffer::from(out)))
    }

    /// Writes the overlay back as a standalone `(60xx,3000)` element and
    /// normalizes bits allocated / bit position (§5.4).
    pub fn store(&self, ds: &mut Dataset, data: SharedBuffer) -> Result<(), Error> {
        let group = self.group;
        let vr = if self.bits_allocated <= 8 {
            Vr::Ob
        } else {
            Vr::Ow
        };
        ds.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(Tag::new(group, 0x3000), vr, data),
        ));
        ds.insert_value(Tag::new(group, 0x0100), 1u16);
        ds.insert_value(Tag::new(group, 0x0102), 0u16);
        Ok(())
    }
}

/// Extracts every embedded overlay into a standalone `(60xx,3000)`
/// element (§5.4). Datasets without overlays are left untouched.
pub fn extract_overlays(ds: &mut Dataset) -> Result<usize, Error> {
    let groups: Vec<u16> = OverlayData::from_dataset(ds)
        .into_iter()
        .filter(|o| o.data.is_none())
        .map(|o| o.group)
        .collect();
    let mut extracted = 0;
    for group in groups {
        let data = OverlayData::extract_embedded(ds, group)?;
        let overlay = OverlayData::from_dataset(ds)
            .into_iter()
            .find(|o| o.group == group)
            .ok_or_else(|| Error::Imaging(ImagingError("overlay group vanished".into())))?;
        overlay.store(ds, data)?;
        extracted += 1;
    }
    Ok(extracted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::buffer::MemoryBuffer;
    use crate::model::transfer_syntax::syntaxes;
    use crate::pixel::{
        BITS_ALLOCATED, BITS_STORED, COLUMNS, PHOTOMETRIC_INTERPRETATION, PIXEL_DATA, ROWS,
    };

    fn base(bits: u16, w: u16, h: u16) -> Dataset {
        let mut ds = Dataset::new();
        // the file meta group is built from the dataset's own SOP class (#8)
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.2");
        ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5.6.7");
        ds.insert_value(ROWS, h);
        ds.insert_value(COLUMNS, w);
        ds.insert_value(BITS_ALLOCATED, bits);
        ds.insert_value(BITS_STORED, bits);
        ds.insert_str(PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
        ds
    }

    fn add_overlay_shape(ds: &mut Dataset, group: u16, w: u16, h: u16, bit_position: u16) {
        ds.insert_value(Tag::new(group, 0x0010), h);
        ds.insert_value(Tag::new(group, 0x0011), w);
        ds.insert_str(Tag::new(group, 0x0040), Vr::Cs, "G");
        ds.insert_value(Tag::new(group, 0x0100), 1u16);
        ds.insert_value(Tag::new(group, 0x0102), bit_position);
    }

    #[test]
    fn standalone_overlay_data_is_used_directly() {
        let mut ds = base(8, 2, 2);
        add_overlay_shape(&mut ds, 0x6000, 2, 2, 0);
        ds.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::new(0x6000, 0x3000),
                Vr::Ob,
                Arc::new(MemoryBuffer::from(vec![0b1100_0000u8])),
            ),
        ));
        let overlays = OverlayData::from_dataset(&ds);
        assert_eq!(overlays.len(), 1);
        let loaded = overlays[0].load(&ds).unwrap();
        assert_eq!(
            loaded.data().unwrap().as_ref(),
            &[0b1100_0000],
            "stored bytes returned as-is"
        );
    }

    #[test]
    fn non_qualifying_groups_are_ignored() {
        let mut ds = base(8, 2, 2);
        // no overlay type
        ds.insert_value(Tag::new(0x6000, 0x0010), 2u16);
        ds.insert_value(Tag::new(0x6000, 0x0011), 2u16);
        assert!(OverlayData::from_dataset(&ds).is_empty());

        // zero shape
        let mut ds = base(8, 2, 2);
        add_overlay_shape(&mut ds, 0x6002, 0, 0, 0);
        assert!(OverlayData::from_dataset(&ds).is_empty());
    }

    #[test]
    fn embedded_overlay_extracts_the_bit_plane_8bit() {
        let mut ds = base(8, 8, 2);
        // 7 stored bits leaves bit 7 for the overlay (PS3.3 C.9.3)
        ds.insert_value(BITS_STORED, 7u16);
        add_overlay_shape(&mut ds, 0x6000, 8, 2, 7);
        // bit 7 set on the first pixel of each row
        let pixels = vec![0x80u8, 0, 0, 0, 0, 0, 0, 0, 0x80, 0, 0, 0, 0, 0, 0, 0];
        ds.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                PIXEL_DATA,
                Vr::Ob,
                Arc::new(MemoryBuffer::from(pixels)),
            ),
        ));
        let overlays = OverlayData::from_dataset(&ds);
        let data = overlays[0].load(&ds).unwrap();
        assert_eq!(
            data.data().unwrap().as_ref(),
            &[0x01, 0x01],
            "one byte per row, LSB first"
        );
    }

    #[test]
    fn embedded_overlay_extracts_the_plane_from_the_requested_frame() {
        let mut ds = base(8, 8, 2);
        ds.insert_value(BITS_STORED, 7u16);
        add_overlay_shape(&mut ds, 0x6000, 8, 2, 7);
        // Two 8x2 frames; only frame 1 has bit 7 set on the first pixel of each row.
        let mut pixels = vec![0u8; 32];
        pixels[16] = 0x80;
        pixels[24] = 0x80;
        ds.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                PIXEL_DATA,
                Vr::Ob,
                Arc::new(MemoryBuffer::from(pixels)),
            ),
        ));
        let overlay = &OverlayData::from_dataset(&ds)[0];
        assert_eq!(
            overlay.load_frame(&ds, 0).unwrap().data().unwrap().as_ref(),
            &[0x00, 0x00]
        );
        assert_eq!(
            overlay.load_frame(&ds, 1).unwrap().data().unwrap().as_ref(),
            &[0x01, 0x01],
            "frame 1's overlay plane"
        );
        assert!(overlay.load_frame(&ds, 2).is_err(), "frame out of range");
    }

    #[test]
    fn embedded_overlay_extracts_the_bit_plane_16bit() {
        let mut ds = base(16, 4, 1);
        ds.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds.insert_value(BITS_STORED, 12u16);
        add_overlay_shape(&mut ds, 0x6000, 4, 1, 12);
        // set bit 12 of pixels 0 and 2
        let mut pixels = vec![0u8; 8];
        let p0 = 1u16 << 12;
        let p2 = 1u16 << 12;
        pixels[0..2].copy_from_slice(&p0.to_le_bytes());
        pixels[4..6].copy_from_slice(&p2.to_le_bytes());
        ds.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                PIXEL_DATA,
                Vr::Ow,
                Arc::new(MemoryBuffer::from(pixels)),
            ),
        ));
        let data = OverlayData::from_dataset(&ds)[0].load(&ds).unwrap();
        assert_eq!(
            data.data().unwrap().as_ref(),
            &[0b0000_0101],
            "pixels 0 and 2 set"
        );
    }

    #[test]
    fn embedded_overlay_reads_big_endian_pixels_after_a_real_file_round_trip() {
        let mut ds = base(16, 2, 1);
        ds.set_transfer_syntax(syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone());
        ds.insert_value(BITS_STORED, 12u16);
        add_overlay_shape(&mut ds, 0x6000, 2, 1, 12);
        // pixel 0 = 0x1000 (bit 12), pixel 1 = 0, in host byte order in memory
        let mut pixels = vec![0u8; 4];
        pixels[0..2].copy_from_slice(&(1u16 << 12).to_ne_bytes());
        ds.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                PIXEL_DATA,
                Vr::Ow,
                Arc::new(MemoryBuffer::from(pixels)),
            ),
        ));
        // Writing big-endian and re-reading must leave host-order pixels, so the
        // embedded overlay still finds bit 12 of pixel 0 (review #3).
        let bytes = crate::io::file::DicomFile::from_dataset(ds)
            .to_bytes(&crate::io::writer::WriteOptions::default())
            .unwrap();
        let back = crate::io::file::DicomFile::from_bytes(
            &bytes,
            &crate::io::file::ReadOptions::default(),
        )
        .unwrap();
        let data = OverlayData::from_dataset(&back.dataset)[0]
            .load(&back.dataset)
            .unwrap();
        assert_eq!(
            data.data().unwrap().as_ref(),
            &[0b0000_0001],
            "big-endian bit 12 of pixel 0 read correctly"
        );
    }

    #[test]
    fn embedded_overlay_requires_decompressed_data() {
        let mut ds = base(8, 8, 2);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        add_overlay_shape(&mut ds, 0x6000, 8, 2, 7);
        let overlays = OverlayData::from_dataset(&ds);
        assert!(
            overlays[0].load(&ds).is_err(),
            "encapsulated data cannot embed overlays"
        );
    }

    #[test]
    fn bit_position_inside_stored_bits_is_rejected() {
        let mut ds = base(8, 4, 1);
        // bits stored 8 means positions 0..7 are real pixel data
        add_overlay_shape(&mut ds, 0x6000, 4, 1, 3);
        ds.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                PIXEL_DATA,
                Vr::Ob,
                Arc::new(MemoryBuffer::from(vec![0u8; 4])),
            ),
        ));
        let overlays = OverlayData::from_dataset(&ds);
        assert!(
            overlays[0].load(&ds).is_err(),
            "#110 rule: not an overlay bit"
        );
    }

    #[test]
    fn extract_overlays_normalizes_the_group() {
        let mut ds = base(8, 8, 1);
        ds.insert_value(BITS_STORED, 7u16);
        add_overlay_shape(&mut ds, 0x6000, 8, 1, 7);
        ds.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                PIXEL_DATA,
                Vr::Ob,
                Arc::new(MemoryBuffer::from(vec![0x80u8, 0, 0, 0, 0, 0, 0, 0])),
            ),
        ));
        assert_eq!(extract_overlays(&mut ds).unwrap(), 1);
        // now standalone
        let overlays = OverlayData::from_dataset(&ds);
        assert!(overlays[0].data.is_some(), "(60xx,3000) was created");
        assert_eq!(overlays[0].bit_position(), 0, "normalized to 0");
        assert_eq!(overlays[0].bits_allocated(), 1);
        // idempotent: nothing left to extract
        assert_eq!(extract_overlays(&mut ds).unwrap(), 0);
    }

    #[test]
    fn overlay_properties_map_the_group_tags() {
        let mut ds = base(8, 4, 2);
        add_overlay_shape(&mut ds, 0x6002, 4, 2, 0);
        // OverlayOrigin is row\column: row 2, column 3 → (x, y) = (3, 2)
        let mut origin_bytes = Vec::new();
        origin_bytes.extend_from_slice(&2i16.to_le_bytes());
        origin_bytes.extend_from_slice(&3i16.to_le_bytes());
        ds.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::new(0x6002, 0x0050),
                Vr::Ss,
                Arc::new(MemoryBuffer::from(origin_bytes)),
            ),
        ));
        ds.insert_value(Tag::new(0x6002, 0x0051), 4u16);
        ds.insert_str(Tag::new(0x6002, 0x1500), Vr::Lo, "LABEL");
        let overlays = OverlayData::from_dataset(&ds);
        assert_eq!(overlays[0].group, 0x6002);
        assert_eq!(overlays[0].origin(), (3, 2), "OverlayOrigin is row\\column");
        assert_eq!(
            overlays[0].frame_origin(),
            4,
            "ImageFrameOrigin is (60xx,0051)"
        );
        assert_eq!(overlays[0].label(), Some("LABEL"));
        assert_eq!(overlays[0].overlay_type(), "G");
    }

    #[test]
    fn overlay_origin_and_frame_origin_parse_from_standard_bytes() {
        fn element(group: u16, element: u16, vr: &[u8; 2], value: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&group.to_le_bytes());
            out.extend_from_slice(&element.to_le_bytes());
            out.extend_from_slice(vr);
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
            out.extend_from_slice(value);
            out
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&element(0x6002, 0x0010, b"US", &2u16.to_le_bytes()));
        bytes.extend_from_slice(&element(0x6002, 0x0011, b"US", &4u16.to_le_bytes()));
        bytes.extend_from_slice(&element(0x6002, 0x0040, b"CS", b"G "));
        // (6002,0050) SS VM2 = row 2 \ column 3
        let mut origin = Vec::new();
        origin.extend_from_slice(&2i16.to_le_bytes());
        origin.extend_from_slice(&3i16.to_le_bytes());
        bytes.extend_from_slice(&element(0x6002, 0x0050, b"SS", &origin));
        // (6002,0051) US VM1 = frame 3
        bytes.extend_from_slice(&element(0x6002, 0x0051, b"US", &3u16.to_le_bytes()));
        let ds = crate::io::dataset_builder::parse_dataset_bytes(
            &bytes,
            &crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN,
        )
        .unwrap();
        let overlays = OverlayData::from_dataset(&ds);
        assert_eq!(overlays[0].origin(), (3, 2), "row\\column becomes (x, y)");
        assert_eq!(overlays[0].frame_origin(), 3);
    }
}
