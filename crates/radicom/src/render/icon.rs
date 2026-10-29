//! DICOM icon image from an Icon Image Sequence item (REND-10;
//! docs/01-architecture.md §10).
//!
//! `(0088,0200)` IconImageSequence items carry a small preview whose pixel data
//! is stored native even when the parent image is compressed. This module parses
//! such an item independently, restricts its photometric interpretation to
//! MONOCHROME1/2 or PALETTE COLOR, forces one frame, and renders it through the
//! same pipeline selection as a normal frame.

use super::RawImage;
use crate::error::{DataError, Error};
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::pixel::photometric::PhotometricInterpretation;
use crate::render::render_frame;

/// `(0088,0200) Icon Image Sequence` (PS3.3 C.7.6.1.1.6).
const ICON_IMAGE_SEQUENCE: Tag = Tag::ICON_IMAGE_SEQUENCE;
/// `(0028,0004) Photometric Interpretation`.
const PHOTOMETRIC_INTERPRETATION: Tag = Tag::PHOTOMETRIC_INTERPRETATION;
/// `(0028,0008) Number of Frames`.
const NUMBER_OF_FRAMES: Tag = Tag::NUMBER_OF_FRAMES;

/// A renderable DICOM icon (fo-dicom `DicomIconImage`).
#[derive(Debug, Clone)]
pub struct IconImage {
    dataset: Dataset,
}

impl IconImage {
    /// Builds an icon from one Icon Image Sequence item (`(0088,0200)`).
    ///
    /// Rejects a photometric interpretation other than MONOCHROME1/2 or PALETTE
    /// COLOR (docs §1.4), and forces NumberOfFrames to 1 so only the first frame
    /// is rendered.
    pub fn try_from_sequence_item(item: &Dataset) -> Result<IconImage, Error> {
        let pi_text = item
            .try_get_str_ref(PHOTOMETRIC_INTERPRETATION)
            .ok_or_else(|| invalid("icon item has no PhotometricInterpretation"))?;
        let pi = PhotometricInterpretation::parse(pi_text);
        if !matches!(
            pi,
            PhotometricInterpretation::Monochrome1
                | PhotometricInterpretation::Monochrome2
                | PhotometricInterpretation::PaletteColor
        ) {
            return Err(invalid(
                "Photometric Interpretation for Icon Image Sequence must be MONOCHROME1, MONOCHROME2 or PALETTE COLOR",
            ));
        }

        // clone so we can normalise without touching the source item, and force
        // a single frame (docs §1.4)
        let mut owned = item.clone();
        owned.insert_str(NUMBER_OF_FRAMES, crate::model::vr::Vr::Is, "1");
        // an icon's pixel data is stored native; make its syntax explicit so the
        // pixel facade never treats it as encapsulated (the parent may be
        // compressed, but the icon item is self-contained)
        if owned.transfer_syntax().is_encapsulated() {
            owned.set_transfer_syntax(
                crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            );
        }

        Ok(IconImage { dataset: owned })
    }

    /// Extracts the icon from a dataset's `(0088,0200)` sequence (must hold
    /// exactly one item), else `None` (fo-dicom `DicomIconImage.TryCreate`).
    pub fn try_from_dataset(dataset: &Dataset) -> Option<IconImage> {
        let sequence = dataset.try_get_sequence(ICON_IMAGE_SEQUENCE)?;
        if sequence.items().len() != 1 {
            return None;
        }
        IconImage::try_from_sequence_item(&sequence.items()[0]).ok()
    }

    /// The icon's width in pixels.
    pub fn width(&self) -> u32 {
        self.dataset.try_get::<u16>(Tag::COLUMNS).unwrap_or(0) as u32
    }

    /// The icon's height in pixels.
    pub fn height(&self) -> u32 {
        self.dataset.try_get::<u16>(Tag::ROWS).unwrap_or(0) as u32
    }

    /// Renders the icon to a [`RawImage`] via the shared pipeline selection.
    pub fn render(&self) -> Result<RawImage, Error> {
        render_frame(&self.dataset, 0)
    }
}

fn invalid(message: &str) -> Error {
    Error::Data(DataError::Other {
        message: format!("invalid icon image: {message}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::vr::Vr;

    fn icon_item(pi: &str, fill: u8) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
        ds.insert_str(PHOTOMETRIC_INTERPRETATION, Vr::Cs, pi);
        ds.insert_value(Tag::ROWS, 4u16); // rows
        ds.insert_value(Tag::COLUMNS, 4u16); // columns
        ds.insert_value(Tag::BITS_ALLOCATED, 8u16);
        ds.insert_value(Tag::BITS_STORED, 8u16);
        ds.insert_value(Tag::HIGH_BIT, 7u16);
        ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
        ds.insert_str(Tag::WINDOW_CENTER, Vr::Ds, "128");
        ds.insert_str(Tag::WINDOW_WIDTH, Vr::Ds, "256");
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vec![fill; 16])),
            ),
        ));
        ds
    }

    #[test]
    fn a_grayscale_icon_renders() {
        let icon = IconImage::try_from_sequence_item(&icon_item("MONOCHROME2", 100)).unwrap();
        assert_eq!(icon.width(), 4);
        assert_eq!(icon.height(), 4);
        let image = icon.render().unwrap();
        assert_eq!((image.width(), image.height()), (4, 4));
    }

    #[test]
    fn palette_color_icon_is_accepted() {
        // validation passes for PALETTE COLOR even without a full LUT here; the
        // constructor only checks the PI (rendering would need the palette)
        assert!(IconImage::try_from_sequence_item(&icon_item("PALETTE COLOR", 0)).is_ok());
    }

    #[test]
    fn an_rgb_icon_is_rejected() {
        let result = IconImage::try_from_sequence_item(&icon_item("RGB", 0));
        assert!(
            result.is_err(),
            "RGB is not a valid icon photometric interpretation"
        );
    }

    #[test]
    fn an_icon_without_a_photometric_interpretation_is_rejected() {
        let mut item = icon_item("MONOCHROME2", 0);
        item.remove(PHOTOMETRIC_INTERPRETATION);
        assert!(IconImage::try_from_sequence_item(&item).is_err());
    }

    #[test]
    fn forcing_a_single_frame_ignores_a_multi_frame_icon() {
        let mut item = icon_item("MONOCHROME2", 50);
        item.insert_str(NUMBER_OF_FRAMES, Vr::Is, "9");
        let icon = IconImage::try_from_sequence_item(&item).unwrap();
        // only frame 0 renders; the dataset reports one frame
        assert_eq!(
            icon.dataset.try_get_str(NUMBER_OF_FRAMES).as_deref(),
            Some("1")
        );
    }

    #[test]
    fn extracting_from_a_dataset_requires_exactly_one_item() {
        let mut parent = Dataset::new();
        let sequence = parent.insert_sequence(ICON_IMAGE_SEQUENCE);
        sequence.push(icon_item("MONOCHROME2", 10));
        assert!(IconImage::try_from_dataset(&parent).is_some());

        // two items → not a valid icon sequence
        let mut parent2 = Dataset::new();
        let sequence2 = parent2.insert_sequence(ICON_IMAGE_SEQUENCE);
        sequence2.push(icon_item("MONOCHROME2", 10));
        sequence2.push(icon_item("MONOCHROME2", 20));
        assert!(IconImage::try_from_dataset(&parent2).is_none());

        // no sequence at all
        assert!(IconImage::try_from_dataset(&Dataset::new()).is_none());
    }

    #[test]
    fn the_icon_sequence_uses_the_standard_tag() {
        // FEAT-02: the icon lives in (0088,0200), not in the old wrong
        // (0000,9113) constant that made the whole feature unreachable.
        assert_eq!(ICON_IMAGE_SEQUENCE.group(), 0x0088);
        assert_eq!(ICON_IMAGE_SEQUENCE.element(), 0x0200);

        let mut wrong = Dataset::new();
        wrong
            .insert_sequence(Tag::new(0x0000, 0x9113))
            .push(icon_item("MONOCHROME2", 10));
        assert!(
            IconImage::try_from_dataset(&wrong).is_none(),
            "a foreign (0000,9113) sequence is not an icon"
        );
    }
}
