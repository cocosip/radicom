//! Print Presentation LUT (PRINT-01; PS3.3 C.11.4, PS3.4 F.1/F.2).
//!
//! The Print Presentation LUT Module lets a Film Box carry either a
//! Presentation LUT Sequence `(2050,0010)` (a descriptor `(0028,3002)` plus data
//! `(0028,3006)`) or a Presentation LUT Shape `(2050,0020)` whose value is
//! `IDENTITY`, `LIN OD` or `INVERSE`.
//!
//! This module models and validates both; it does not render. `print::render`
//! applies the shape (`IDENTITY` unchanged, `INVERSE` inverted) and, when a
//! sequence is present, maps each output byte through the table. `LIN OD` is
//! modelled and validated but not applied (it needs the printer's optical-density
//! response, which this renderer does not have).

use crate::error::{DataError, Error};
use crate::model::dataset::Dataset;
use crate::model::item::DicomItem;
use crate::model::tag::Tag;
use crate::model::vr::Vr;

/// Presentation LUT Shape `(2050,0020)` values (PS3.3 C.11.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationLutShape {
    /// `IDENTITY`: values are used unchanged.
    Identity,
    /// `LIN OD`: linear optical density (film response).
    LinOd,
    /// `INVERSE`: values are inverted before display.
    Inverse,
}

impl PresentationLutShape {
    /// Parses the CS value; trims and uppercases, rejects anything else.
    pub fn parse(text: &str) -> Result<Self, Error> {
        match text.trim().to_ascii_uppercase().as_str() {
            "IDENTITY" => Ok(PresentationLutShape::Identity),
            "LIN OD" => Ok(PresentationLutShape::LinOd),
            "INVERSE" => Ok(PresentationLutShape::Inverse),
            other => Err(data_error(format!(
                "unknown Presentation LUT Shape \"{other}\""
            ))),
        }
    }

    /// The canonical CS spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            PresentationLutShape::Identity => "IDENTITY",
            PresentationLutShape::LinOd => "LIN OD",
            PresentationLutShape::Inverse => "INVERSE",
        }
    }
}

/// One item of the Presentation LUT Sequence: descriptor `(0028,3002)` plus
/// data `(0028,3006)`, mirroring the VOI/modality LUT descriptor semantics.
#[derive(Debug, Clone)]
pub struct PresentationLutTable {
    first_mapped: i32,
    bits: u16,
    entries: Vec<i32>,
}

impl PresentationLutTable {
    /// Builds a table from a sequence item. Errors when the descriptor or data
    /// is missing/malformed or the entry count does not match.
    pub fn from_dataset(item: &Dataset) -> Result<Self, Error> {
        let descriptor = item
            .get_element(Tag::LUT_DESCRIPTOR)
            .map_err(|_| data_error("Presentation LUT Sequence item has no LUT Descriptor"))?;
        let desc = descriptor
            .value()
            .as_buffer()
            .ok_or_else(|| data_error("Presentation LUT Descriptor is not byte-backed"))?
            .data();
        if desc.len() < 6 {
            return Err(data_error(
                "Presentation LUT Descriptor is shorter than 3 US values",
            ));
        }
        let raw_entries = match descriptor.vr() {
            Vr::Ss => i16::from_le_bytes([desc[0], desc[1]]) as i32,
            _ => u16::from_le_bytes([desc[0], desc[1]]) as i32,
        };
        // fo-dicom treats a nonsensical negative entry count as its absolute value.
        let count = if raw_entries == 0 {
            65536usize
        } else {
            raw_entries.unsigned_abs() as usize
        };
        let first_mapped = i16::from_le_bytes([desc[2], desc[3]]) as i32;
        let bits = u16::from_le_bytes([desc[4], desc[5]]);
        if !matches!(bits, 8 | 16) {
            return Err(data_error(format!(
                "Presentation LUT Descriptor bit depth {bits} is not 8 or 16"
            )));
        }

        let data = element_bytes(item, Tag::LUT_DATA)
            .ok_or_else(|| data_error("Presentation LUT Sequence item has no LUT Data"))?;
        let data_vr = item
            .get_element(Tag::LUT_DATA)
            .map(|e| e.vr())
            .unwrap_or(Vr::Ow);

        let mut entries = Vec::with_capacity(count);
        if data.len() == count {
            // 8-bit entries carry one byte each
            entries.extend(data[..count].iter().map(|b| *b as i32));
        } else {
            if data.len() < count * 2 {
                return Err(data_error(format!(
                    "Presentation LUT has {count} entries but only {} data bytes",
                    data.len()
                )));
            }
            for i in 0..count {
                let pair = [data[i * 2], data[i * 2 + 1]];
                entries.push(if data_vr == Vr::Ss {
                    i16::from_le_bytes(pair) as i32
                } else {
                    u16::from_le_bytes(pair) as i32
                });
            }
        }
        Ok(PresentationLutTable {
            first_mapped,
            bits,
            entries,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn bits(&self) -> u16 {
        self.bits
    }

    pub fn first_mapped(&self) -> i32 {
        self.first_mapped
    }

    /// The raw entry for `input`, or `None` outside `first_mapped..`.
    pub fn value_at(&self, input: i32) -> Option<i32> {
        let index = input - self.first_mapped;
        if index < 0 || index as usize >= self.entries.len() {
            return None;
        }
        Some(self.entries[index as usize])
    }

    /// Maps an 8-bit input value through the table to an 8-bit output byte,
    /// rescaling from the descriptor's `bits` output range. Out-of-range input
    /// maps to 0 (black).
    pub fn map_byte(&self, input: i32) -> u8 {
        let Some(value) = self.value_at(input) else {
            return 0;
        };
        let max = ((1u32 << self.bits) - 1) as f64;
        ((value as f64).clamp(0.0, max) * 255.0 / max).round() as u8
    }
}

/// The presentation LUT carried by a Film Box: an optional table and/or shape.
#[derive(Debug, Clone, Default)]
pub struct PresentationLut {
    shape: Option<PresentationLutShape>,
    table: Option<PresentationLutTable>,
}

impl PresentationLut {
    /// Parses `(2050,0010)` and `(2050,0020)` from a film-box dataset.
    ///
    /// Missing attributes yield `None` fields; a malformed shape, an empty
    /// sequence or an inconsistent table is an error.
    pub fn from_dataset(dataset: &Dataset) -> Result<Self, Error> {
        let shape = match dataset.try_get_str(Tag::PRESENTATION_LUT_SHAPE) {
            Some(text) => Some(PresentationLutShape::parse(&text)?),
            None => None,
        };
        let table = match dataset.try_get_sequence(Tag::PRESENTATION_LUT_SEQUENCE) {
            Some(sequence) => {
                let item = sequence
                    .items()
                    .first()
                    .ok_or_else(|| data_error("Presentation LUT Sequence has no item"))?;
                Some(PresentationLutTable::from_dataset(item)?)
            }
            None => None,
        };
        Ok(PresentationLut { shape, table })
    }

    pub fn shape(&self) -> Option<PresentationLutShape> {
        self.shape
    }

    pub fn table(&self) -> Option<&PresentationLutTable> {
        self.table.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.shape.is_none() && self.table.is_none()
    }
}

fn element_bytes(dataset: &Dataset, tag: Tag) -> Option<Vec<u8>> {
    match dataset.item(&tag)? {
        DicomItem::Element(element) => element
            .value()
            .as_buffer()
            .map(|buffer| buffer.data().to_vec()),
        _ => None,
    }
}

fn data_error(message: impl Into<String>) -> Error {
    Error::Data(DataError::Other {
        message: message.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::buffer::MemoryBuffer;
    use crate::model::element::DicomElement;
    use crate::print::models::FilmBox;
    use std::sync::Arc;

    #[test]
    fn each_presentation_lut_shape_parses_from_a_film_box() {
        for (text, expected) in [
            ("IDENTITY", PresentationLutShape::Identity),
            ("LIN OD", PresentationLutShape::LinOd),
            ("INVERSE", PresentationLutShape::Inverse),
        ] {
            let mut film = Dataset::new();
            film.insert_str(Tag::PRESENTATION_LUT_SHAPE, Vr::Cs, text);
            let lut = FilmBox::from_dataset(film).presentation_lut().unwrap();
            assert_eq!(lut.shape(), Some(expected));
            assert!(lut.table().is_none());
        }
        // canonical spelling round-trips, and an unknown value is an error
        assert_eq!(PresentationLutShape::LinOd.as_str(), "LIN OD");
        let mut bad = Dataset::new();
        bad.insert_str(Tag::PRESENTATION_LUT_SHAPE, Vr::Cs, "NOPE");
        assert!(FilmBox::from_dataset(bad).presentation_lut().is_err());
    }

    #[test]
    fn a_presentation_lut_sequence_is_parsed() {
        // 8-bit, 4 entries, first mapped 0: values 0, 85, 170, 255
        let mut item = Dataset::new();
        let descriptor = [4u8, 0x00, 0x00, 0x00, 0x08, 0x00]; // US 4, SS 0, US 8
        item.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::LUT_DESCRIPTOR,
            Vr::Us,
            Arc::new(MemoryBuffer::from(descriptor.to_vec())),
        )));
        item.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::LUT_DATA,
            Vr::Ow,
            Arc::new(MemoryBuffer::from(vec![0u8, 85, 170, 255])),
        )));

        let mut film = Dataset::new();
        film.insert_sequence(Tag::PRESENTATION_LUT_SEQUENCE)
            .push(item);
        let lut = FilmBox::from_dataset(film).presentation_lut().unwrap();
        let table = lut.table().expect("table parsed");
        assert_eq!(table.len(), 4);
        assert_eq!(table.bits(), 8);
        assert_eq!(table.first_mapped(), 0);
        assert_eq!(table.value_at(2), Some(170));
        assert_eq!(table.map_byte(3), 255);
        assert_eq!(table.value_at(9), None);
    }

    #[test]
    fn a_truncated_presentation_lut_sequence_errors() {
        let mut item = Dataset::new();
        // descriptor says 4 entries at 16 bits, but only one 2-byte entry follows
        let descriptor = [4u8, 0x00, 0x00, 0x00, 0x10, 0x00];
        item.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::LUT_DESCRIPTOR,
            Vr::Us,
            Arc::new(MemoryBuffer::from(descriptor.to_vec())),
        )));
        item.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::LUT_DATA,
            Vr::Ow,
            Arc::new(MemoryBuffer::from(vec![0u8, 0])),
        )));
        let mut film = Dataset::new();
        film.insert_sequence(Tag::PRESENTATION_LUT_SEQUENCE)
            .push(item);
        assert!(FilmBox::from_dataset(film).presentation_lut().is_err());
    }

    #[test]
    fn a_us_descriptor_keeps_a_large_entry_count_unsigned() {
        let mut item = Dataset::new();
        // US entry count 65535 must not become signed -1 (and then count 1).
        let descriptor = [0xFF, 0xFF, 0x00, 0x00, 0x08, 0x00];
        item.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::LUT_DESCRIPTOR,
            Vr::Us,
            Arc::new(MemoryBuffer::from(descriptor.to_vec())),
        )));
        item.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::LUT_DATA,
            Vr::Ow,
            Arc::new(MemoryBuffer::from(vec![0u8])),
        )));

        let mut film = Dataset::new();
        film.insert_sequence(Tag::PRESENTATION_LUT_SEQUENCE)
            .push(item);
        assert!(FilmBox::from_dataset(film).presentation_lut().is_err());
    }
}
