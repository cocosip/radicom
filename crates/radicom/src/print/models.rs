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
use crate::print::presentation_lut::{PresentationLut, PresentationLutShape};

/// Tags used by the print data model (§12.4).
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
    /// (2010,0080) Smoothing Type.
    pub const SMOOTHING_TYPE: Tag = Tag::SMOOTHING_TYPE;
    /// (2010,0100) Border Density.
    pub const BORDER_DENSITY: Tag = Tag::BORDER_DENSITY;
    /// (2010,0110) Empty Image Density.
    pub const EMPTY_IMAGE_DENSITY: Tag = Tag::EMPTY_IMAGE_DENSITY;
    /// (2010,0120) Minimum Density.
    pub const MIN_DENSITY: Tag = Tag::MIN_DENSITY;
    /// (2010,0130) Maximum Density.
    pub const MAX_DENSITY: Tag = Tag::MAX_DENSITY;
    /// (2010,0140) Trim.
    pub const TRIM: Tag = Tag::TRIM;
    /// (2010,0510) Referenced Image Box Sequence (in FilmBox).
    pub const REFERENCED_IMAGE_BOX_SEQUENCE: Tag = Tag::REFERENCED_IMAGE_BOX_SEQUENCE;
    /// (2020,0010) Image Position.
    pub const IMAGE_POSITION: Tag = Tag::IMAGE_BOX_POSITION;
    /// (2020,0020) Polarity.
    pub const POLARITY: Tag = Tag::POLARITY;
    /// (2020,0030) Requested Image Size.
    pub const REQUESTED_IMAGE_SIZE: Tag = Tag::REQUESTED_IMAGE_SIZE;
    /// (2050,0010) Presentation LUT Sequence.
    pub const PRESENTATION_LUT_SEQUENCE: Tag = Tag::PRESENTATION_LUT_SEQUENCE;
    /// (2050,0020) Presentation LUT Shape.
    pub const PRESENTATION_LUT_SHAPE: Tag = Tag::PRESENTATION_LUT_SHAPE;
    /// (0028,3002) LUT Descriptor (inside a Presentation LUT Sequence item).
    pub const LUT_DESCRIPTOR: Tag = Tag::LUT_DESCRIPTOR;
    /// (0028,3006) LUT Data (inside a Presentation LUT Sequence item).
    pub const LUT_DATA: Tag = Tag::LUT_DATA;
}

/// Film orientation `(2010,0040)` (PS3.3 C.13.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilmOrientation {
    /// `PORTRAIT`: the film is laid out in its nominal portrait dimensions.
    #[default]
    Portrait,
    /// `LANDSCAPE`: the film is rotated, swapping width and height.
    Landscape,
}

impl FilmOrientation {
    /// Parses the CS value; anything else is an error.
    pub fn parse(text: &str) -> Result<Self, Error> {
        match text.trim().to_ascii_uppercase().as_str() {
            "PORTRAIT" => Ok(FilmOrientation::Portrait),
            "LANDSCAPE" => Ok(FilmOrientation::Landscape),
            other => Err(format_error(other)),
        }
    }

    /// The canonical CS spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            FilmOrientation::Portrait => "PORTRAIT",
            FilmOrientation::Landscape => "LANDSCAPE",
        }
    }
}

/// A print film session (§12.4): film destination/label/medium/priority/copies.
#[derive(Debug, Clone)]
pub struct FilmSession {
    dataset: Dataset,
}

impl FilmSession {
    pub fn new() -> Self {
        FilmSession {
            dataset: Dataset::new(),
        }
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

    pub fn set_copies(&mut self, copies: u16) {
        self.dataset.insert_value(tags::NUMBER_OF_COPIES, copies);
    }

    pub fn copies(&self) -> Option<u16> {
        self.dataset.try_get(tags::NUMBER_OF_COPIES)
    }

    pub fn set_label(&mut self, label: impl Into<String>) {
        self.dataset
            .insert_str(tags::FILM_SESSION_LABEL, Vr::Sh, label);
    }

    pub fn label(&self) -> Option<String> {
        self.dataset.try_get_str(tags::FILM_SESSION_LABEL)
    }

    pub fn set_priority(&mut self, priority: impl Into<String>) {
        self.dataset
            .insert_str(tags::PRINT_PRIORITY, Vr::Cs, priority);
    }

    pub fn priority(&self) -> Option<String> {
        self.dataset.try_get_str(tags::PRINT_PRIORITY)
    }

    pub fn set_medium_type(&mut self, medium: impl Into<String>) {
        self.dataset.insert_str(tags::MEDIUM_TYPE, Vr::Cs, medium);
    }

    pub fn medium_type(&self) -> Option<String> {
        self.dataset.try_get_str(tags::MEDIUM_TYPE)
    }

    pub fn set_destination(&mut self, destination: impl Into<String>) {
        self.dataset
            .insert_str(tags::FILM_DESTINATION, Vr::Cs, destination);
    }

    pub fn destination(&self) -> Option<String> {
        self.dataset.try_get_str(tags::FILM_DESTINATION)
    }
}

impl Default for FilmSession {
    fn default() -> Self {
        Self::new()
    }
}

/// A film box (§12.4): one film's display format and print attributes.
#[derive(Debug, Clone)]
pub struct FilmBox {
    dataset: Dataset,
}

impl FilmBox {
    pub fn new() -> Self {
        FilmBox {
            dataset: Dataset::new(),
        }
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

    /// Sets the display format string and validates it parses (§12.4).
    pub fn set_image_display_format(&mut self, format: impl Into<String>) -> Result<(), Error> {
        let text = format.into();
        ImageDisplayFormat::parse(&text)?;
        self.dataset
            .insert_str(tags::IMAGE_DISPLAY_FORMAT, Vr::Cs, text);
        Ok(())
    }

    /// The parsed display format, if any.
    pub fn image_display_format(&self) -> Option<ImageDisplayFormat> {
        self.dataset
            .try_get_str(tags::IMAGE_DISPLAY_FORMAT)
            .and_then(|text| ImageDisplayFormat::parse(&text).ok())
    }

    /// Sets the film size string and validates it parses (§12.4).
    pub fn set_film_size(&mut self, size: impl Into<String>) -> Result<(), Error> {
        let text = size.into();
        if FilmSize::parse(&text).is_none() {
            return Err(format_error(&text));
        }
        self.dataset.insert_str(tags::FILM_SIZE_ID, Vr::Cs, text);
        Ok(())
    }

    /// The film's physical size, if recognised.
    pub fn film_size(&self) -> Option<FilmSize> {
        FilmSize::parse(&self.dataset.try_get_str(tags::FILM_SIZE_ID)?)
    }

    /// The film size after applying `FilmOrientation` (PS3.3 C.13.3.1):
    /// LANDSCAPE swaps width and height.
    pub fn effective_film_size(&self) -> Option<FilmSize> {
        let size = self.film_size()?;
        Some(match self.film_orientation() {
            FilmOrientation::Portrait => size,
            FilmOrientation::Landscape => FilmSize::new(size.height_mm, size.width_mm),
        })
    }

    /// Sets the film orientation (PORTRAIT by default).
    pub fn set_film_orientation(&mut self, orientation: FilmOrientation) {
        self.dataset
            .insert_str(tags::FILM_ORIENTATION, Vr::Cs, orientation.as_str());
    }

    /// The film orientation; missing or malformed values read as PORTRAIT.
    pub fn film_orientation(&self) -> FilmOrientation {
        self.dataset
            .try_get_str(tags::FILM_ORIENTATION)
            .and_then(|text| FilmOrientation::parse(&text).ok())
            .unwrap_or_default()
    }

    pub fn set_trim(&mut self, trim: bool) {
        self.dataset
            .insert_str(tags::TRIM, Vr::Cs, if trim { "YES" } else { "NO" });
    }

    pub fn trim(&self) -> bool {
        self.dataset.try_get_str(tags::TRIM).as_deref() == Some("YES")
    }

    /// Sets `(2010,0080)` Smoothing Type (Table C.13-3). The standard leaves the
    /// values to the Conformance Statement; the renderer recognises `SMOOTH`,
    /// `MEDIUM` and `SHARP` and prints anything else unfiltered. An image box
    /// value overrides this one (Table C.13-5).
    pub fn set_smoothing_type(&mut self, value: impl Into<String>) {
        self.dataset.insert_str(tags::SMOOTHING_TYPE, Vr::Cs, value);
    }

    pub fn smoothing_type(&self) -> Option<String> {
        self.dataset.try_get_str(tags::SMOOTHING_TYPE)
    }

    /// Sets `(2010,0100)` Border Density (Table C.13-3): the density of the film
    /// areas surrounding and between images. `BLACK`, `WHITE` or hundredths of
    /// optical density; absent leaves those areas at the empty-image density.
    pub fn set_border_density(&mut self, value: impl Into<String>) {
        self.dataset.insert_str(tags::BORDER_DENSITY, Vr::Cs, value);
    }

    pub fn border_density(&self) -> Option<String> {
        self.dataset.try_get_str(tags::BORDER_DENSITY)
    }

    /// Sets `(2010,0110)` Empty Image Density (Table C.13-3): the density of an
    /// image box area on the film that contains no image. Same value forms as
    /// [`Self::set_border_density`].
    pub fn set_empty_image_density(&mut self, value: impl Into<String>) {
        self.dataset
            .insert_str(tags::EMPTY_IMAGE_DENSITY, Vr::Cs, value);
    }

    pub fn empty_image_density(&self) -> Option<String> {
        self.dataset.try_get_str(tags::EMPTY_IMAGE_DENSITY)
    }

    /// Sets `(2010,0120)` Min Density (hundredths of optical density).
    pub fn set_min_density(&mut self, value: u16) {
        self.dataset.insert_value(tags::MIN_DENSITY, value);
    }

    pub fn min_density(&self) -> Option<u16> {
        self.dataset.try_get(tags::MIN_DENSITY)
    }

    /// Sets `(2010,0130)` Max Density (hundredths of optical density).
    pub fn set_max_density(&mut self, value: u16) {
        self.dataset.insert_value(tags::MAX_DENSITY, value);
    }

    pub fn max_density(&self) -> Option<u16> {
        self.dataset.try_get(tags::MAX_DENSITY)
    }

    /// Lays out this film's image cells, inset by `margin` mm (§12.4).
    ///
    /// Returns an empty vector when the display format or film size is missing
    /// or unparseable, rather than guessing a grid. The grid is laid out on the
    /// orientation-adjusted film size, so LANDSCAPE rotates the cells too.
    pub fn layout(&self, margin: f64) -> Vec<LayoutCell> {
        match (self.image_display_format(), self.effective_film_size()) {
            (Some(format), Some(size)) => format.layout(size, margin),
            _ => Vec::new(),
        }
    }

    /// Initialises the ReferencedImageBoxSequence with `count` image boxes,
    /// numbering positions 1..=count (§12.4, `initialize`).
    pub fn initialize_image_boxes(&mut self, count: u32) {
        let sequence = self
            .dataset
            .insert_sequence(tags::REFERENCED_IMAGE_BOX_SEQUENCE);
        sequence.items_mut().clear();
        for position in 1..=count {
            let mut boxed = Dataset::new();
            boxed.insert_value(tags::IMAGE_POSITION, position as u16);
            sequence.push(boxed);
        }
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
            .map(|sequence| {
                sequence
                    .items()
                    .iter()
                    .cloned()
                    .map(ImageBox::from_dataset)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The Presentation LUT carried by this film box (PS3.3 C.11.4): a
    /// `(2050,0010)` sequence and/or a `(2050,0020)` shape. Malformed values are
    /// an error rather than silently ignored.
    pub fn presentation_lut(&self) -> Result<PresentationLut, Error> {
        PresentationLut::from_dataset(&self.dataset)
    }

    /// Sets the Presentation LUT Shape `(2050,0020)`.
    pub fn set_presentation_lut_shape(&mut self, shape: PresentationLutShape) {
        self.dataset
            .insert_str(tags::PRESENTATION_LUT_SHAPE, Vr::Cs, shape.as_str());
    }
}

impl Default for FilmBox {
    fn default() -> Self {
        Self::new()
    }
}

/// An image box (§12.4): a cell's position, polarity and image payload.
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

    pub fn set_position(&mut self, position: u16) {
        self.dataset.insert_value(tags::IMAGE_POSITION, position);
    }

    /// Polarity `NORMAL` (default) or `REVERSE` (§12.4).
    pub fn polarity(&self) -> bool {
        // true = reversed
        self.dataset.try_get_str(tags::POLARITY).as_deref() == Some("REVERSE")
    }

    pub fn set_polarity(&mut self, reverse: bool) {
        self.dataset.insert_str(
            tags::POLARITY,
            Vr::Cs,
            if reverse { "REVERSE" } else { "NORMAL" },
        );
    }

    pub fn set_magnification_type(&mut self, value: impl Into<String>) {
        self.dataset
            .insert_str(tags::MAGNIFICATION_TYPE, Vr::Cs, value);
    }

    pub fn magnification_type(&self) -> Option<String> {
        self.dataset.try_get_str(tags::MAGNIFICATION_TYPE)
    }

    /// Sets `(2010,0080)` on this image box, overriding the film box value
    /// (Table C.13-5). See [`FilmBox::set_smoothing_type`] for the values.
    pub fn set_smoothing_type(&mut self, value: impl Into<String>) {
        self.dataset.insert_str(tags::SMOOTHING_TYPE, Vr::Cs, value);
    }

    pub fn smoothing_type(&self) -> Option<String> {
        self.dataset.try_get_str(tags::SMOOTHING_TYPE)
    }

    pub fn set_requested_image_size(&mut self, value: f64) {
        self.dataset
            .insert_str(tags::REQUESTED_IMAGE_SIZE, Vr::Ds, value.to_string());
    }

    pub fn requested_image_size(&self) -> Option<f64> {
        self.dataset
            .try_get_str(tags::REQUESTED_IMAGE_SIZE)
            .and_then(|value| value.trim().parse().ok())
    }

    /// Attaches one image (a dataset holding pixel data) to the image sequence.
    pub fn push_image(&mut self, image: Dataset) {
        self.dataset
            .insert_sequence(Tag::BASIC_GRAYSCALE_IMAGE_SEQUENCE)
            .push(image);
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
    Error::Data(crate::error::DataError::Other {
        message: format!("unknown film size \"{text}\""),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_film_session_carries_its_attributes() {
        let mut session = FilmSession::new();
        session.set_copies(2);
        session.set_label("FS-1");
        session.set_priority("HIGH");
        session.set_medium_type("PAPER");
        session.set_destination("PROCESSOR");
        assert_eq!(session.copies(), Some(2));
        assert_eq!(session.label().as_deref(), Some("FS-1"));
        assert_eq!(session.priority().as_deref(), Some("HIGH"));
        assert_eq!(session.medium_type().as_deref(), Some("PAPER"));
        assert_eq!(session.destination().as_deref(), Some("PROCESSOR"));
    }

    #[test]
    fn wrappers_round_trip_their_datasets() {
        let mut session = FilmSession::new();
        session.set_copies(3);
        assert_eq!(session.dataset().try_get(tags::NUMBER_OF_COPIES), Some(3));
        assert!(session.into_dataset().contains(&tags::NUMBER_OF_COPIES));
        assert!(FilmSession::from_dataset(Dataset::new())
            .dataset()
            .items()
            .next()
            .is_none());
        assert!(FilmSession::default().copies().is_none());

        let mut film = FilmBox::new();
        film.set_film_size("A4").unwrap();
        assert_eq!(
            film.dataset().try_get_str(tags::FILM_SIZE_ID).as_deref(),
            Some("A4")
        );
        assert!(film.into_dataset().contains(&tags::FILM_SIZE_ID));
        assert!(FilmBox::default().film_size().is_none());

        let mut image_box = ImageBox::from_dataset(Dataset::new());
        image_box.set_position(4);
        assert_eq!(image_box.position(), Some(4));
        assert!(image_box.dataset().contains(&tags::IMAGE_POSITION));
    }

    #[test]
    fn a_film_box_validates_the_display_format() {
        let mut film = FilmBox::new();
        film.set_image_display_format("STANDARD\\2,2").unwrap();
        assert_eq!(
            film.image_display_format(),
            Some(ImageDisplayFormat::Standard {
                columns: 2,
                rows: 2
            })
        );
        assert!(
            film.set_image_display_format("BOGUS").is_err(),
            "invalid format is rejected"
        );
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
        let sequence = film
            .dataset()
            .try_get_sequence(tags::REFERENCED_IMAGE_BOX_SEQUENCE)
            .unwrap();
        assert_eq!(
            sequence.items()[0].try_get::<u16>(tags::IMAGE_POSITION),
            Some(1)
        );
        assert_eq!(
            sequence.items()[2].try_get::<u16>(tags::IMAGE_POSITION),
            Some(3)
        );
    }

    #[test]
    fn trim_round_trips() {
        let mut film = FilmBox::new();
        assert!(!film.trim());
        film.set_trim(true);
        assert!(film.trim());
    }

    #[test]
    fn the_film_box_carries_the_presentation_densities() {
        // Table C.13-3: these are Film Box attributes, not Image Box ones.
        let mut film = FilmBox::new();
        assert_eq!(film.border_density(), None);
        assert_eq!(film.smoothing_type(), None);
        assert_eq!(film.min_density(), None);
        assert_eq!(film.max_density(), None);

        film.set_border_density("WHITE");
        film.set_smoothing_type("SMOOTH");
        film.set_min_density(10);
        film.set_max_density(200);
        assert_eq!(film.border_density().as_deref(), Some("WHITE"));
        assert_eq!(film.smoothing_type().as_deref(), Some("SMOOTH"));
        assert_eq!(film.min_density(), Some(10));
        assert_eq!(film.max_density(), Some(200));
        // and they land on the PS3.6 tags, not on some neighbour
        assert_eq!(
            film.dataset().try_get_str(tags::BORDER_DENSITY).as_deref(),
            Some("WHITE")
        );
        assert_eq!(film.dataset().try_get::<u16>(tags::MIN_DENSITY), Some(10));
        assert_eq!(film.dataset().try_get::<u16>(tags::MAX_DENSITY), Some(200));
    }

    #[test]
    fn an_image_box_reports_position_polarity_and_images() {
        let mut image_box = ImageBox::from_dataset(Dataset::new());
        image_box.set_position(2);
        image_box.set_polarity(true);
        assert_eq!(image_box.position(), Some(2));
        assert!(image_box.polarity(), "REVERSE maps to true");

        image_box.set_magnification_type("REPLICATE");
        image_box.set_smoothing_type("MEDIUM");
        image_box.set_requested_image_size(25.5);
        assert_eq!(image_box.magnification_type().as_deref(), Some("REPLICATE"));
        assert_eq!(image_box.smoothing_type().as_deref(), Some("MEDIUM"));
        assert_eq!(image_box.requested_image_size(), Some(25.5));

        image_box.push_image(Dataset::new());
        assert_eq!(image_box.image_count(), 1);
    }

    #[test]
    fn landscape_swaps_the_film_and_cell_dimensions() {
        let mut film = FilmBox::new();
        film.set_image_display_format("STANDARD\\2,2").unwrap();
        film.set_film_size("A4").unwrap(); // 210 x 297 portrait
        assert_eq!(film.film_orientation(), FilmOrientation::Portrait);
        let portrait = film.layout(10.0);
        assert!((portrait[0].width - (210.0 - 20.0) / 2.0).abs() < 1e-9);

        film.set_film_orientation(FilmOrientation::Landscape);
        assert_eq!(film.film_orientation(), FilmOrientation::Landscape);
        assert_eq!(
            film.effective_film_size(),
            Some(FilmSize::new(297.0, 210.0))
        );
        let landscape = film.layout(10.0);
        // the grid is now wider than tall: each cell is (297-20)/2 wide
        assert!((landscape[0].width - (297.0 - 20.0) / 2.0).abs() < 1e-9);
        assert!((landscape[0].height - (210.0 - 20.0) / 2.0).abs() < 1e-9);

        // malformed and missing values read as PORTRAIT
        film.dataset
            .insert_str(tags::FILM_ORIENTATION, Vr::Cs, "SIDEWAYS");
        assert_eq!(film.film_orientation(), FilmOrientation::Portrait);
        assert!(FilmOrientation::parse("landscape ").is_ok());
        assert!(FilmOrientation::parse("").is_err());
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
            (tags::SMOOTHING_TYPE, "SmoothingType"),
            (tags::BORDER_DENSITY, "BorderDensity"),
            (tags::EMPTY_IMAGE_DENSITY, "EmptyImageDensity"),
            (tags::MIN_DENSITY, "MinDensity"),
            (tags::MAX_DENSITY, "MaxDensity"),
            (tags::TRIM, "Trim"),
            (
                tags::REFERENCED_IMAGE_BOX_SEQUENCE,
                "ReferencedImageBoxSequence",
            ),
            (tags::IMAGE_POSITION, "ImageBoxPosition"),
            (tags::POLARITY, "Polarity"),
            (tags::REQUESTED_IMAGE_SIZE, "RequestedImageSize"),
        ];
        let dictionary = crate::default_dictionary();
        for (tag, keyword) in expected {
            assert_eq!(
                dictionary.entry(&tag).keyword,
                keyword,
                "{tag} is not {keyword}"
            );
        }
        // the two tags the print module got wrong, spelled out
        assert_eq!(tags::EMPTY_IMAGE_DENSITY, Tag::new(0x2010, 0x0110));
        assert_eq!(
            tags::REFERENCED_IMAGE_BOX_SEQUENCE,
            Tag::new(0x2010, 0x0510)
        );
    }
}
