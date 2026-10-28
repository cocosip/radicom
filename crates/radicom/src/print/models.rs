//! Printing data models (PRINT-01; docs/01-architecture.md §12).
//!
//! `FilmSession`, `FilmBox` and `ImageBox` wrap a `Dataset` each, exposing the
//! common attributes and the film-box layout maths. They are the document model
//! the print SCU would publish; `FilmBox::render_to_image` (PRINT-02) is
//! feature-gated behind `render`.

use crate::error::Error;
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::model::vr::Vr;
use crate::print::layout::{FilmSize, ImageDisplayFormat, LayoutCell};

/// Tags used by the print data model (§5).
pub mod tags {
    use crate::model::tag::Tag;

    /// (2000,0010) Number of Copies.
    pub const NUMBER_OF_COPIES: Tag = Tag::NUMBER_OF_COPIES;
    /// (2000,0020) Print Priority.
    pub const PRINT_PRIORITY: Tag = Tag::PRINT_PRIORITY;
    /// (2000,0030) Medium Type.
    pub const MEDIUM_TYPE: Tag = Tag::MEDIUM_TYPE;
    /// (2000,0040) Film Destination.
    pub const FILM_DESTINATION: Tag = Tag::FILM_DESTINATION;
    /// (2000,0050) Film Session Label.
    pub const FILM_SESSION_LABEL: Tag = Tag::FILM_SESSION_LABEL;
    /// (2010,0010) Image Display Format.
    pub const IMAGE_DISPLAY_FORMAT: Tag = Tag::IMAGE_DISPLAY_FORMAT;
    /// (2010,0040) Film Orientation.
    pub const FILM_ORIENTATION: Tag = Tag::FILM_ORIENTATION;
    /// (2010,0050) Film Size ID.
    pub const FILM_SIZE_ID: Tag = Tag::FILM_SIZE_ID;
    /// (2010,0060) Magnification Type.
    pub const MAGNIFICATION_TYPE: Tag = Tag::MAGNIFICATION_TYPE;
    /// (2010,0100) Border Density.
    pub const BORDER_DENSITY: Tag = Tag::BORDER_DENSITY;
    /// (2010,0110) Empty Image Density.
    pub const EMPTY_IMAGE_DENSITY: Tag = Tag::EMPTY_IMAGE_DENSITY;
    /// (2010,0140) Trim.
    pub const TRIM: Tag = Tag::TRIM;
    /// (2010,0510) Referenced Image Box Sequence (in FilmBox).
    pub const REFERENCED_IMAGE_BOX_SEQUENCE: Tag = Tag::REFERENCED_IMAGE_BOX_SEQUENCE;
    /// (2020,0010) Image Position.
    pub const IMAGE_POSITION: Tag = Tag::IMAGE_BOX_POSITION;
    /// (2020,0020) Polarity.
    pub const POLARITY: Tag = Tag::POLARITY;
}

/// A print film session (§5): film destination/label/medium/priority/copies.
#[derive(Debug, Clone)]
pub struct FilmSession {
    dataset: Dataset,
}

impl FilmSession {
    pub fn new() -> Self {
        FilmSession { dataset: Dataset::new() }
    }

    pub fn from_dataset(dataset: Dataset) -> Self {
        FilmSession { dataset }
    }

    pub fn dataset(&self) -> &Dataset {
        &self.dataset
    }

    pub fn into_dataset(self) -> Dataset {
        self.dataset
    }

    pub fn set_copies(&mut self, copies: u16) -> &mut Self {
        self.dataset.insert_value(tags::NUMBER_OF_COPIES, copies);
        self
    }

    pub fn copies(&self) -> Option<u16> {
        self.dataset.try_get(tags::NUMBER_OF_COPIES)
    }

    pub fn set_label(&mut self, label: impl Into<String>) -> &mut Self {
        self.dataset.insert_str(tags::FILM_SESSION_LABEL, Vr::Sh, label);
        self
    }

    pub fn label(&self) -> Option<String> {
        self.dataset.try_get_str(tags::FILM_SESSION_LABEL)
    }

    pub fn set_priority(&mut self, priority: impl Into<String>) -> &mut Self {
        self.dataset.insert_str(tags::PRINT_PRIORITY, Vr::Cs, priority);
        self
    }
}

impl Default for FilmSession {
    fn default() -> Self {
        Self::new()
    }
}

/// A film box (§5): one film's display format and print attributes.
#[derive(Debug, Clone)]
pub struct FilmBox {
    dataset: Dataset,
}

impl FilmBox {
    pub fn new() -> Self {
        FilmBox { dataset: Dataset::new() }
    }

    pub fn from_dataset(dataset: Dataset) -> Self {
        FilmBox { dataset }
    }

    pub fn dataset(&self) -> &Dataset {
        &self.dataset
    }

    pub fn into_dataset(self) -> Dataset {
        self.dataset
    }

    /// Sets the display format string and validates it parses (§5).
    pub fn set_image_display_format(&mut self, format: impl Into<String>) -> Result<&mut Self, Error> {
        let text = format.into();
        ImageDisplayFormat::parse(&text)?;
        self.dataset.insert_str(tags::IMAGE_DISPLAY_FORMAT, Vr::Cs, text);
        Ok(self)
    }

    /// The parsed display format, if any.
    pub fn image_display_format(&self) -> Option<ImageDisplayFormat> {
        self.dataset
            .try_get_str(tags::IMAGE_DISPLAY_FORMAT)
            .and_then(|text| ImageDisplayFormat::parse(&text).ok())
    }

    /// Sets the film size string and validates it parses (§5).
    pub fn set_film_size(&mut self, size: impl Into<String>) -> Result<&mut Self, Error> {
        let text = size.into();
        if FilmSize::parse(&text).is_none() {
            return Err(format_error(&text));
        }
        self.dataset.insert_str(tags::FILM_SIZE_ID, Vr::Cs, text);
        Ok(self)
    }

    /// The film's physical size, if recognised.
    pub fn film_size(&self) -> Option<FilmSize> {
        FilmSize::parse(&self.dataset.try_get_str(tags::FILM_SIZE_ID)?)
    }

    pub fn set_trim(&mut self, trim: bool) -> &mut Self {
        self.dataset.insert_str(tags::TRIM, Vr::Cs, if trim { "YES" } else { "NO" });
        self
    }

    pub fn trim(&self) -> bool {
        self.dataset.try_get_str(tags::TRIM).as_deref() == Some("YES")
    }

    /// Lays out this film's image cells, inset by `margin` mm (§5).
    ///
    /// Returns an empty vector when the display format or film size is missing
    /// or unparseable, rather than guessing a grid.
    pub fn layout(&self, margin: f64) -> Vec<LayoutCell> {
        match (self.image_display_format(), self.film_size()) {
            (Some(format), Some(size)) => format.layout(size, margin),
            _ => Vec::new(),
        }
    }

    /// Initialises the ReferencedImageBoxSequence with `count` image boxes,
    /// numbering positions 1..=count (§5, `initialize()`).
    pub fn initialize_image_boxes(&mut self, count: u32) -> &mut Self {
        let sequence = self.dataset.insert_sequence(tags::REFERENCED_IMAGE_BOX_SEQUENCE);
        sequence.items_mut().clear();
        for position in 1..=count {
            let mut boxed = Dataset::new();
            boxed.insert_value(tags::IMAGE_POSITION, position as u16);
            sequence.push(boxed);
        }
        self
    }

    /// Number of image boxes currently in the ReferencedImageBoxSequence.
    pub fn image_box_count(&self) -> usize {
        self.dataset
            .try_get_sequence(tags::REFERENCED_IMAGE_BOX_SEQUENCE)
            .map(|sequence| sequence.items().len())
            .unwrap_or(0)
    }

    /// The image boxes as owned [`ImageBox`] views (clones the item datasets).
    pub fn image_boxes(&self) -> Vec<ImageBox> {
        self.dataset
            .try_get_sequence(tags::REFERENCED_IMAGE_BOX_SEQUENCE)
            .map(|sequence| sequence.items().iter().cloned().map(ImageBox::from_dataset).collect())
            .unwrap_or_default()
    }
}

impl Default for FilmBox {
    fn default() -> Self {
        Self::new()
    }
}

/// An image box (§5): a cell's position, polarity and image payload.
#[derive(Debug, Clone)]
pub struct ImageBox {
    dataset: Dataset,
}

impl ImageBox {
    pub fn from_dataset(dataset: Dataset) -> Self {
        ImageBox { dataset }
    }

    pub fn dataset(&self) -> &Dataset {
        &self.dataset
    }

    pub fn position(&self) -> Option<u16> {
        self.dataset.try_get(tags::IMAGE_POSITION)
    }

    /// Polarity `NORMAL` (default) or `REVERSE` (§5).
    pub fn polarity(&self) -> bool {
        // true = reversed
        self.dataset.try_get_str(tags::POLARITY).as_deref() == Some("REVERSE")
    }

    /// Attaches one image (a dataset holding pixel data) to the image sequence.
    pub fn push_image(&mut self, image: Dataset) -> &mut Self {
        self.dataset
            .insert_sequence(Tag::BASIC_GRAYSCALE_IMAGE_SEQUENCE)
            .push(image);
        self
    }

    pub fn image_count(&self) -> usize {
        self.dataset
            .try_get_sequence(Tag::BASIC_GRAYSCALE_IMAGE_SEQUENCE)
            .map(|sequence| sequence.items().len())
            .unwrap_or(0)
    }

    /// The image datasets attached to this box (its ImageSequence items).
    pub fn images(&self) -> &[Dataset] {
        self.dataset
            .try_get_sequence(Tag::BASIC_GRAYSCALE_IMAGE_SEQUENCE)
            .map(|sequence| sequence.items())
            .unwrap_or(&[])
    }
}

fn format_error(text: &str) -> Error {
    Error::Data(crate::error::DataError::Other { message: format!("unknown film size \"{text}\"") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_film_session_carries_its_attributes() {
        let mut session = FilmSession::new();
        session.set_copies(2).set_label("FS-1");
        assert_eq!(session.copies(), Some(2));
        assert_eq!(session.label().as_deref(), Some("FS-1"));
    }

    #[test]
    fn a_film_box_validates_the_display_format() {
        let mut film = FilmBox::new();
        film.set_image_display_format("STANDARD\\2,2").unwrap();
        assert_eq!(film.image_display_format(), Some(ImageDisplayFormat::Standard { columns: 2, rows: 2 }));
        assert!(film.set_image_display_format("BOGUS").is_err(), "invalid format is rejected");
    }

    #[test]
    fn a_film_box_validates_the_film_size() {
        let mut film = FilmBox::new();
        film.set_film_size("A4").unwrap();
        assert_eq!(film.film_size(), Some(FilmSize::new(210.0, 297.0)));
        assert!(film.set_film_size("nosuch").is_err());
    }

    #[test]
    fn a_film_box_lays_out_its_cells() {
        let mut film = FilmBox::new();
        film.set_image_display_format("STANDARD\\2,2").unwrap();
        film.set_film_size("24CMX30CM").unwrap();
        let cells = film.layout(10.0);
        assert_eq!(cells.len(), 4);
        // 240x300mm film, 10mm margin → usable 220x280
        assert!((cells[0].width - 110.0).abs() < 1e-9);
    }

    #[test]
    fn a_film_box_without_format_or_size_has_no_layout() {
        let film = FilmBox::new();
        assert!(film.layout(10.0).is_empty());
    }

    #[test]
    fn initialize_creates_numbered_image_boxes() {
        let mut film = FilmBox::new();
        film.initialize_image_boxes(3);
        assert_eq!(film.image_box_count(), 3);
        let sequence = film.dataset().try_get_sequence(tags::REFERENCED_IMAGE_BOX_SEQUENCE).unwrap();
        assert_eq!(sequence.items()[0].try_get::<u16>(tags::IMAGE_POSITION), Some(1));
        assert_eq!(sequence.items()[2].try_get::<u16>(tags::IMAGE_POSITION), Some(3));
    }

    #[test]
    fn trim_round_trips() {
        let mut film = FilmBox::new();
        assert!(!film.trim());
        film.set_trim(true);
        assert!(film.trim());
    }

    #[test]
    fn an_image_box_reports_position_polarity_and_images() {
        let mut dataset = Dataset::new();
        dataset.insert_value(tags::IMAGE_POSITION, 2u16);
        dataset.insert_str(tags::POLARITY, Vr::Cs, "REVERSE");
        let mut image_box = ImageBox::from_dataset(dataset);
        assert_eq!(image_box.position(), Some(2));
        assert!(image_box.polarity(), "REVERSE maps to true");

        image_box.push_image(Dataset::new());
        assert_eq!(image_box.image_count(), 1);
    }

    #[test]
    fn every_print_tag_names_the_attribute_it_claims() {
        // PS3.6 keyword looked up by tag: a constant with the wrong tag number
        // resolves to a different attribute (this is how (2010,0120) MinDensity
        // was mistaken for EmptyImageDensity).
        let expected = [
            (tags::NUMBER_OF_COPIES, "NumberOfCopies"),
            (tags::PRINT_PRIORITY, "PrintPriority"),
            (tags::MEDIUM_TYPE, "MediumType"),
            (tags::FILM_DESTINATION, "FilmDestination"),
            (tags::FILM_SESSION_LABEL, "FilmSessionLabel"),
            (tags::IMAGE_DISPLAY_FORMAT, "ImageDisplayFormat"),
            (tags::FILM_ORIENTATION, "FilmOrientation"),
            (tags::FILM_SIZE_ID, "FilmSizeID"),
            (tags::MAGNIFICATION_TYPE, "MagnificationType"),
            (tags::BORDER_DENSITY, "BorderDensity"),
            (tags::EMPTY_IMAGE_DENSITY, "EmptyImageDensity"),
            (tags::TRIM, "Trim"),
            (tags::REFERENCED_IMAGE_BOX_SEQUENCE, "ReferencedImageBoxSequence"),
            (tags::IMAGE_POSITION, "ImageBoxPosition"),
            (tags::POLARITY, "Polarity"),
        ];
        let dictionary = crate::default_dictionary();
        for (tag, keyword) in expected {
            assert_eq!(dictionary.entry(&tag).keyword, keyword, "{tag} is not {keyword}");
        }
        // the two tags the print module got wrong, spelled out
        assert_eq!(tags::EMPTY_IMAGE_DENSITY, Tag::new(0x2010, 0x0110));
        assert_eq!(tags::REFERENCED_IMAGE_BOX_SEQUENCE, Tag::new(0x2010, 0x0510));
    }
}
