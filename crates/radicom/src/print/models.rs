//! Printing data models (PRINT-01; docs/01-architecture.md §12).
//!
//! `FilmSession`, `FilmBox` and `ImageBox` wrap a `Dataset` each, exposing the
//! common attributes and the film-box layout maths. They are the document model
//! the print SCU would publish; `FilmBox::render_to_image` (PRINT-02) is
//! feature-gated behind `render`.

use crate::error::{DataError, Error};
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::model::uid::Uid;
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
    /// (2010,015E) Illumination.
    pub const ILLUMINATION: Tag = Tag::ILLUMINATION;
    /// (2010,0160) Reflected Ambient Light.
    pub const REFLECTED_AMBIENT_LIGHT: Tag = Tag::REFLECTED_AMBIENT_LIGHT;
    /// (2020,0050) Requested Resolution ID (Image Box; fo-dicom also exposes it
    /// on `FilmBox`).
    pub const REQUESTED_RESOLUTION_ID: Tag = Tag::REQUESTED_RESOLUTION_ID;
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
    /// (2050,0500) Referenced Presentation LUT Sequence (in FilmBox).
    pub const REFERENCED_PRESENTATION_LUT_SEQUENCE: Tag = Tag::REFERENCED_PRESENTATION_LUT_SEQUENCE;
    /// (0008,0016) SOP Class UID of an Image Box / Presentation LUT instance.
    pub const SOP_CLASS_UID: Tag = Tag::SOP_CLASS_UID;
    /// (0008,0018) SOP Instance UID of an Image Box / Presentation LUT instance.
    pub const SOP_INSTANCE_UID: Tag = Tag::SOP_INSTANCE_UID;
    /// (0008,1150) Referenced SOP Class UID (inside a reference item).
    pub const REFERENCED_SOP_CLASS_UID: Tag = Tag::REFERENCED_SOP_CLASS_UID;
    /// (0008,1155) Referenced SOP Instance UID (inside a reference item).
    pub const REFERENCED_SOP_INSTANCE_UID: Tag = Tag::REFERENCED_SOP_INSTANCE_UID;
    /// (2020,0110) Basic Grayscale Image Sequence.
    pub const BASIC_GRAYSCALE_IMAGE_SEQUENCE: Tag = Tag::BASIC_GRAYSCALE_IMAGE_SEQUENCE;
    /// (2020,0111) Basic Color Image Sequence.
    pub const BASIC_COLOR_IMAGE_SEQUENCE: Tag = Tag::BASIC_COLOR_IMAGE_SEQUENCE;
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
///
/// A session also owns the Presentation LUT SOP instances created for it
/// (`fo-dicom FilmSession.CreatePresentationLut`), so a film box can reference
/// one by SOP Instance UID (`(2050,0500)`) instead of embedding the table.
#[derive(Debug, Clone)]
pub struct FilmSession {
    dataset: Dataset,
    presentation_luts: Vec<PresentationLut>,
}

impl FilmSession {
    pub fn new() -> Self {
        FilmSession {
            dataset: Dataset::new(),
            presentation_luts: Vec::new(),
        }
    }

    pub fn from_dataset(dataset: Dataset) -> Self {
        FilmSession {
            dataset,
            presentation_luts: Vec::new(),
        }
    }

    pub fn dataset(&self) -> &Dataset {
        &self.dataset
    }

    /// Consumes the session and returns its dataset. The Presentation LUT SOP
    /// instances are separate objects, so they are not written here.
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

    /// Registers a standalone Presentation LUT SOP instance with this session
    /// (fo-dicom `FilmSession.CreatePresentationLut`). A film box references it
    /// through [`FilmBox::set_referenced_presentation_lut`].
    pub fn add_presentation_lut(&mut self, lut: PresentationLut) {
        self.presentation_luts.push(lut);
    }

    /// The Presentation LUT SOP instances created for this session.
    pub fn presentation_luts(&self) -> &[PresentationLut] {
        &self.presentation_luts
    }

    /// Finds a registered Presentation LUT by its SOP Instance UID.
    pub fn find_presentation_lut(&self, sop_instance_uid: &str) -> Option<&PresentationLut> {
        self.presentation_luts
            .iter()
            .find(|lut| lut.sop_instance_uid().as_deref() == Some(sop_instance_uid))
    }

    /// Removes a registered Presentation LUT, returning whether it existed.
    pub fn remove_presentation_lut(&mut self, sop_instance_uid: &str) -> bool {
        let before = self.presentation_luts.len();
        self.presentation_luts
            .retain(|lut| lut.sop_instance_uid().as_deref() != Some(sop_instance_uid));
        self.presentation_luts.len() != before
    }
}

impl Default for FilmSession {
    fn default() -> Self {
        Self::new()
    }
}

/// A film box (§12.4): one film's display format and print attributes.
///
/// The film box dataset carries only *references* to its Image Box SOP
/// instances (`(2010,0510)`); the image boxes themselves are separate SOP
/// instances kept in memory, like fo-dicom's `FilmBox.BasicImageBoxes`.
#[derive(Debug, Clone)]
pub struct FilmBox {
    dataset: Dataset,
    image_boxes: Vec<ImageBox>,
}

impl FilmBox {
    pub fn new() -> Self {
        FilmBox {
            dataset: Dataset::new(),
            image_boxes: Vec::new(),
        }
    }

    pub fn from_dataset(dataset: Dataset) -> Self {
        FilmBox {
            dataset,
            image_boxes: Vec::new(),
        }
    }

    pub fn dataset(&self) -> &Dataset {
        &self.dataset
    }

    /// Consumes the film box and returns its dataset. The Image Box SOP
    /// instances are separate objects: only their `(2010,0510)` references are
    /// written, so a parsed-back film box has no image payloads until the
    /// image boxes are reattached with [`Self::add_image_box`].
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

    /// Sets `(2010,0060)` Magnification Type (Table C.13-3). `REPLICATE` selects
    /// nearest-neighbour scaling; anything else (including the `BILINEAR`
    /// default) uses bilinear interpolation. An image box value overrides this
    /// one (Table C.13-5).
    pub fn set_magnification_type(&mut self, value: impl Into<String>) {
        self.dataset
            .insert_str(tags::MAGNIFICATION_TYPE, Vr::Cs, value);
    }

    /// The film box's Magnification Type, `BILINEAR` when absent (fo-dicom
    /// `FilmBox.MagnificationType`).
    pub fn magnification_type(&self) -> String {
        self.dataset
            .try_get_str(tags::MAGNIFICATION_TYPE)
            .unwrap_or_else(|| "BILINEAR".to_string())
    }

    /// Sets `(2010,015E)` Illumination (US).
    pub fn set_illumination(&mut self, value: u16) {
        self.dataset.insert_value(tags::ILLUMINATION, value);
    }

    pub fn illumination(&self) -> Option<u16> {
        self.dataset.try_get(tags::ILLUMINATION)
    }

    /// Sets `(2010,0160)` Reflected Ambient Light (US).
    pub fn set_reflected_ambient_light(&mut self, value: u16) {
        self.dataset
            .insert_value(tags::REFLECTED_AMBIENT_LIGHT, value);
    }

    pub fn reflected_ambient_light(&self) -> Option<u16> {
        self.dataset.try_get(tags::REFLECTED_AMBIENT_LIGHT)
    }

    /// Sets `(2020,0050)` Requested Resolution ID (CS).
    pub fn set_requested_resolution_id(&mut self, value: impl Into<String>) {
        self.dataset
            .insert_str(tags::REQUESTED_RESOLUTION_ID, Vr::Cs, value);
    }

    pub fn requested_resolution_id(&self) -> Option<String> {
        self.dataset.try_get_str(tags::REQUESTED_RESOLUTION_ID)
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

    /// Creates a Basic Grayscale or Basic Color Image Box SOP instance and adds
    /// its reference to `(2010,0510)` (fo-dicom `FilmBox.CreateImageBox`).
    ///
    /// The image box dataset is a separate SOP instance, so it is kept in
    /// memory (like fo-dicom's `BasicImageBoxes`); the film box dataset carries
    /// only the `(0008,1150)`/`(0008,1155)` reference.
    pub fn create_image_box(&mut self, position: u16, color: bool) -> &mut ImageBox {
        let index = self.image_boxes.len();
        self.add_image_box(ImageBox::new(position, color));
        &mut self.image_boxes[index]
    }

    /// Adds an existing Image Box SOP instance to this film box and appends its
    /// `(2010,0510)` reference item.
    pub fn add_image_box(&mut self, image_box: ImageBox) {
        self.push_image_box_reference(&image_box);
        self.image_boxes.push(image_box);
    }

    fn push_image_box_reference(&mut self, image_box: &ImageBox) {
        let mut item = Dataset::new();
        if let Some(uid) = image_box.sop_class_uid() {
            item.insert_str(tags::REFERENCED_SOP_CLASS_UID, Vr::Ui, uid);
        }
        if let Some(uid) = image_box.sop_instance_uid() {
            item.insert_str(tags::REFERENCED_SOP_INSTANCE_UID, Vr::Ui, uid);
        }
        match self
            .dataset
            .sequence_mut(tags::REFERENCED_IMAGE_BOX_SEQUENCE)
        {
            Some(sequence) => sequence.push(item),
            None => {
                self.dataset
                    .insert_sequence(tags::REFERENCED_IMAGE_BOX_SEQUENCE)
                    .push(item);
            }
        }
    }

    /// Initialises the ReferencedImageBoxSequence with `count` Basic Grayscale
    /// Image Box SOP instances numbered 1..=count (§12.4, `initialize`).
    pub fn initialize_image_boxes(&mut self, count: u32) {
        self.image_boxes.clear();
        self.dataset
            .insert_sequence(tags::REFERENCED_IMAGE_BOX_SEQUENCE)
            .items_mut()
            .clear();
        for position in 1..=count {
            let image_box = ImageBox::new(position as u16, false);
            self.push_image_box_reference(&image_box);
            self.image_boxes.push(image_box);
        }
    }

    /// Number of image boxes currently in the film box.
    pub fn image_box_count(&self) -> usize {
        if self.image_boxes.is_empty() {
            self.dataset
                .try_get_sequence(tags::REFERENCED_IMAGE_BOX_SEQUENCE)
                .map(|sequence| sequence.items().len())
                .unwrap_or(0)
        } else {
            self.image_boxes.len()
        }
    }

    /// The image boxes as owned [`ImageBox`] views.
    ///
    /// Image boxes created through this film box come from memory; for a film
    /// box parsed from a dataset, the reference items are read as radicom's
    /// legacy carrier (their nested image sequence).
    pub fn image_boxes(&self) -> Vec<ImageBox> {
        if !self.image_boxes.is_empty() {
            return self.image_boxes.clone();
        }
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

    /// Sets `(2050,0500)` Referenced Presentation LUT Sequence to a standalone
    /// Presentation LUT SOP instance (fo-dicom
    /// `FilmBox.ReferencedPresentationLutSequence`).
    pub fn set_referenced_presentation_lut(&mut self, sop_instance_uid: impl Into<String>) {
        let mut item = Dataset::new();
        item.insert_str(
            tags::REFERENCED_SOP_CLASS_UID,
            Vr::Ui,
            crate::uids::PRESENTATION_LUT,
        );
        item.insert_str(tags::REFERENCED_SOP_INSTANCE_UID, Vr::Ui, sop_instance_uid);
        let sequence = self
            .dataset
            .insert_sequence(tags::REFERENCED_PRESENTATION_LUT_SEQUENCE);
        sequence.items_mut().clear();
        sequence.push(item);
    }

    /// The SOP Instance UID from `(2050,0500)`, when present.
    pub fn referenced_presentation_lut_uid(&self) -> Option<String> {
        self.dataset
            .try_get_sequence(tags::REFERENCED_PRESENTATION_LUT_SEQUENCE)?
            .items()
            .first()?
            .try_get_str(tags::REFERENCED_SOP_INSTANCE_UID)
    }

    /// Resolves the Presentation LUT this film box prints with: the standalone
    /// SOP instance referenced through `(2050,0500)`, looked up in `session`,
    /// otherwise the embedded `(2050,0010)` table / `(2050,0020)` shape.
    pub fn resolve_presentation_lut(
        &self,
        session: &FilmSession,
    ) -> Result<PresentationLut, Error> {
        if let Some(uid) = self.referenced_presentation_lut_uid() {
            return session.find_presentation_lut(&uid).cloned().ok_or_else(|| {
                Error::Data(DataError::Other {
                    message: format!(
                        "referenced Presentation LUT {uid} is not in the film session"
                    ),
                })
            });
        }
        self.presentation_lut()
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
    /// Creates a Basic Image Box SOP instance (fo-dicom `ImageBox`): a generated
    /// SOP Instance UID, the grayscale or colour SOP Class UID and the position.
    pub fn new(position: u16, color: bool) -> Self {
        let mut dataset = Dataset::new();
        dataset.insert_str(tags::SOP_CLASS_UID, Vr::Ui, Self::sop_class_for(color));
        dataset.insert_str(tags::SOP_INSTANCE_UID, Vr::Ui, Uid::generate_value());
        dataset.insert_value(tags::IMAGE_POSITION, position);
        ImageBox { dataset }
    }

    /// The SOP Class UID for a grayscale or colour image box.
    fn sop_class_for(color: bool) -> &'static str {
        if color {
            crate::uids::BASIC_COLOR_IMAGE_BOX
        } else {
            crate::uids::BASIC_GRAYSCALE_IMAGE_BOX
        }
    }

    pub fn from_dataset(dataset: Dataset) -> Self {
        ImageBox { dataset }
    }

    pub fn dataset(&self) -> &Dataset {
        &self.dataset
    }

    /// `(0008,0016)` SOP Class UID.
    pub fn sop_class_uid(&self) -> Option<String> {
        self.dataset.try_get_str(tags::SOP_CLASS_UID)
    }

    pub fn set_sop_class_uid(&mut self, uid: impl Into<String>) {
        self.dataset.insert_str(tags::SOP_CLASS_UID, Vr::Ui, uid);
    }

    /// `(0008,0018)` SOP Instance UID.
    pub fn sop_instance_uid(&self) -> Option<String> {
        self.dataset.try_get_str(tags::SOP_INSTANCE_UID)
    }

    pub fn set_sop_instance_uid(&mut self, uid: impl Into<String>) {
        self.dataset.insert_str(tags::SOP_INSTANCE_UID, Vr::Ui, uid);
    }

    /// True for a Basic Color Image Box (its SOP Class UID, or a colour image
    /// sequence already attached).
    pub fn is_color(&self) -> bool {
        self.sop_class_uid().as_deref() == Some(crate::uids::BASIC_COLOR_IMAGE_BOX)
            || self
                .dataset
                .try_get_sequence(tags::BASIC_COLOR_IMAGE_SEQUENCE)
                .is_some()
    }

    /// Switches this box between Basic Grayscale and Basic Color (§12.4).
    pub fn set_color(&mut self, color: bool) {
        self.set_sop_class_uid(Self::sop_class_for(color));
    }

    /// The image sequence tag this box writes: `(2020,0111)` for colour,
    /// `(2020,0110)` otherwise (PS3.3 C.13.6).
    fn image_sequence_tag(&self) -> Tag {
        if self.is_color() {
            tags::BASIC_COLOR_IMAGE_SEQUENCE
        } else {
            tags::BASIC_GRAYSCALE_IMAGE_SEQUENCE
        }
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

    /// Sets `(2010,0120)` Min Density on this image box (Table C.13-5); it
    /// overrides the film box value for this cell.
    pub fn set_min_density(&mut self, value: u16) {
        self.dataset.insert_value(tags::MIN_DENSITY, value);
    }

    pub fn min_density(&self) -> Option<u16> {
        self.dataset.try_get(tags::MIN_DENSITY)
    }

    /// Sets `(2010,0130)` Max Density on this image box (Table C.13-5).
    pub fn set_max_density(&mut self, value: u16) {
        self.dataset.insert_value(tags::MAX_DENSITY, value);
    }

    pub fn max_density(&self) -> Option<u16> {
        self.dataset.try_get(tags::MAX_DENSITY)
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

    /// Attaches one image to this box's image sequence: `(2020,0111)` for a
    /// Basic Color Image Box, `(2020,0110)` otherwise.
    pub fn push_image(&mut self, image: Dataset) {
        let tag = self.image_sequence_tag();
        self.dataset.insert_sequence(tag).push(image);
    }

    /// Number of images attached to this box.
    pub fn image_count(&self) -> usize {
        self.images().len()
    }

    /// The image datasets attached to this box (its image sequence items).
    pub fn images(&self) -> &[Dataset] {
        self.dataset
            .try_get_sequence(self.image_sequence_tag())
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
    fn image_boxes_are_sop_instances_referenced_by_the_film_box() {
        // fo-dicom FilmBox.CreateImageBox: one Basic Image Box SOP instance per
        // cell, referenced from (2010,0510) by SOP Class/Instance UID.
        let mut film = FilmBox::new();
        film.initialize_image_boxes(3);
        assert_eq!(film.image_box_count(), 3);
        let boxes = film.image_boxes();
        let sequence = film
            .dataset()
            .try_get_sequence(tags::REFERENCED_IMAGE_BOX_SEQUENCE)
            .unwrap();
        assert_eq!(sequence.items().len(), 3);
        for (index, image_box) in boxes.iter().enumerate() {
            assert_eq!(image_box.position(), Some(index as u16 + 1));
            assert_eq!(
                image_box.sop_class_uid().as_deref(),
                Some(crate::uids::BASIC_GRAYSCALE_IMAGE_BOX)
            );
            let reference = &sequence.items()[index];
            assert_eq!(
                reference
                    .try_get_str(tags::REFERENCED_SOP_CLASS_UID)
                    .as_deref(),
                image_box.sop_class_uid().as_deref()
            );
            assert_eq!(
                reference.try_get_str(tags::REFERENCED_SOP_INSTANCE_UID),
                image_box.sop_instance_uid()
            );
        }
    }

    #[test]
    fn a_color_image_box_uses_the_color_sop_class_and_sequence() {
        let mut film = FilmBox::new();
        film.create_image_box(1, true).push_image(Dataset::new());
        let boxes = film.image_boxes();
        assert_eq!(
            boxes[0].sop_class_uid().as_deref(),
            Some(crate::uids::BASIC_COLOR_IMAGE_BOX)
        );
        assert!(boxes[0].is_color());
        assert_eq!(boxes[0].image_count(), 1);
        assert!(boxes[0]
            .dataset()
            .try_get_sequence(tags::BASIC_COLOR_IMAGE_SEQUENCE)
            .is_some());
        assert!(boxes[0]
            .dataset()
            .try_get_sequence(tags::BASIC_GRAYSCALE_IMAGE_SEQUENCE)
            .is_none());
        // the reference item points at the Basic Color Image Box SOP class
        let reference = &film
            .dataset()
            .try_get_sequence(tags::REFERENCED_IMAGE_BOX_SEQUENCE)
            .unwrap()
            .items()[0];
        assert_eq!(
            reference
                .try_get_str(tags::REFERENCED_SOP_CLASS_UID)
                .as_deref(),
            Some(crate::uids::BASIC_COLOR_IMAGE_BOX)
        );
    }

    #[test]
    fn a_film_box_resolves_a_referenced_presentation_lut_from_the_session() {
        // fo-dicom: a film box may reference a standalone Presentation LUT SOP
        // instance through (2050,0500) instead of embedding (2050,0010).
        let lut =
            PresentationLut::from_dataset(&presentation_lut_instance("1.2.3.4.99", &[0])).unwrap();
        assert_eq!(lut.sop_instance_uid().as_deref(), Some("1.2.3.4.99"));
        let mut session = FilmSession::new();
        session.add_presentation_lut(lut);
        assert!(session.find_presentation_lut("1.2.3.4.99").is_some());

        let mut film = FilmBox::new();
        film.set_referenced_presentation_lut("1.2.3.4.99");
        assert_eq!(
            film.referenced_presentation_lut_uid().as_deref(),
            Some("1.2.3.4.99")
        );
        let reference = &film
            .dataset()
            .try_get_sequence(tags::REFERENCED_PRESENTATION_LUT_SEQUENCE)
            .unwrap()
            .items()[0];
        assert_eq!(
            reference
                .try_get_str(tags::REFERENCED_SOP_CLASS_UID)
                .as_deref(),
            Some(crate::uids::PRESENTATION_LUT)
        );
        assert_eq!(
            reference.try_get_str(tags::REFERENCED_SOP_INSTANCE_UID),
            Some("1.2.3.4.99".to_string())
        );

        let resolved = film.resolve_presentation_lut(&session).unwrap();
        assert_eq!(resolved.table().unwrap().map_byte(128), 0);

        // an unresolved reference is an error rather than a silent identity
        assert!(film.resolve_presentation_lut(&FilmSession::new()).is_err());
        assert!(session.remove_presentation_lut("1.2.3.4.99"));
        assert!(session.find_presentation_lut("1.2.3.4.99").is_none());
    }

    /// A Presentation LUT SOP instance dataset: SOP Class/Instance plus a
    /// `(2050,0010)` table of 16-bit `entries`.
    fn presentation_lut_instance(uid: &str, entries: &[u16]) -> Dataset {
        use crate::model::buffer::MemoryBuffer;
        use crate::model::element::DicomElement;
        use crate::model::item::DicomItem;
        use std::sync::Arc;

        let mut dataset = Dataset::new();
        dataset.insert_str(tags::SOP_CLASS_UID, Vr::Ui, crate::uids::PRESENTATION_LUT);
        dataset.insert_str(tags::SOP_INSTANCE_UID, Vr::Ui, uid);
        let mut item = Dataset::new();
        let mut descriptor = Vec::new();
        for value in [entries.len() as u16, 0, 16] {
            descriptor.extend_from_slice(&value.to_le_bytes());
        }
        item.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::LUT_DESCRIPTOR,
            Vr::Us,
            Arc::new(MemoryBuffer::from(descriptor)),
        )));
        let mut data = Vec::new();
        for value in entries {
            data.extend_from_slice(&value.to_le_bytes());
        }
        item.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::LUT_DATA,
            Vr::Ow,
            Arc::new(MemoryBuffer::from(data)),
        )));
        dataset
            .insert_sequence(Tag::PRESENTATION_LUT_SEQUENCE)
            .push(item);
        dataset
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
