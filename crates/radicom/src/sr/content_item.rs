//! SR content items (SR-01; docs/01-architecture.md §12).
//!
//! An SR is a tree of content items. Each item is one `Dataset` inside a
//! `ContentSequence` (0040,A730) carrying a `ConceptNameCodeSequence`
//! (0040,A043), a `ValueType` (0040,A040), a `RelationshipType` (0040,A010,
//! absent on the root) and a type-specific value tag. This module builds and
//! reads that shape with the value type checked at construction time.

use crate::error::{DataError, Error};
use crate::model::dataset::Dataset;
use crate::model::vr::Vr;
use crate::sr::code_item::CodeItem;

/// Tags used by a content item (§12.3).
pub mod tags {
    use crate::model::tag::Tag;

    /// (0040,A730) ContentSequence.
    pub const CONTENT_SEQUENCE: Tag = Tag::CONTENT_SEQUENCE;
    /// (0040,A040) ValueType.
    pub const VALUE_TYPE: Tag = Tag::VALUE_TYPE;
    /// (0040,A010) RelationshipType.
    pub const RELATIONSHIP_TYPE: Tag = Tag::RELATIONSHIP_TYPE;
    /// (0040,A043) ConceptNameCodeSequence.
    pub const CONCEPT_NAME: Tag = Tag::CONCEPT_NAME_CODE_SEQUENCE;
    /// (0040,A160) TextValue.
    pub const TEXT_VALUE: Tag = Tag::TEXT_VALUE;
    /// (0040,A123) PersonName.
    pub const PERSON_NAME: Tag = Tag::PERSON_NAME;
    /// (0040,A120) DateTime.
    pub const DATE_TIME: Tag = Tag::DATE_TIME;
    /// (0040,A121) Date.
    pub const DATE: Tag = Tag::DATE;
    /// (0040,A122) Time.
    pub const TIME: Tag = Tag::TIME;
    /// (0040,A124) UID.
    pub const UID: Tag = Tag::UID;
    /// (0040,A168) ConceptCodeSequence.
    pub const CONCEPT_CODE_SEQUENCE: Tag = Tag::CONCEPT_CODE_SEQUENCE;
    /// (0040,A300) MeasuredValueSequence.
    pub const MEASURED_VALUE_SEQUENCE: Tag = Tag::MEASURED_VALUE_SEQUENCE;
    /// (0040,A30A) NumericValue (DS), inside a MeasuredValueSequence item.
    pub const NUMERIC_VALUE: Tag = Tag::NUMERIC_VALUE;
    /// (0040,08EA) MeasurementUnitsCodeSequence.
    pub const MEASUREMENT_UNITS_CODE_SEQUENCE: Tag = Tag::MEASUREMENT_UNITS_CODE_SEQUENCE;
    /// (0008,1199) ReferencedSOPSequence.
    pub const REFERENCED_SOP_SEQUENCE: Tag = Tag::REFERENCED_SOP_SEQUENCE;
    /// (0008,1150) ReferencedSOPClassUID.
    pub const REFERENCED_SOP_CLASS_UID: Tag = Tag::REFERENCED_SOP_CLASS_UID;
    /// (0008,1155) ReferencedSOPInstanceUID.
    pub const REFERENCED_SOP_INSTANCE_UID: Tag = Tag::REFERENCED_SOP_INSTANCE_UID;
    /// (0040,A050) ContinuityOfContent of a CONTAINER item.
    pub const CONTINUITY_OF_CONTENT: Tag = Tag::CONTINUITY_OF_CONTENT;
    /// (0070,0022) GraphicData: the 2D pairs or 3D triplets of a SCOORD item.
    pub const GRAPHIC_DATA: Tag = Tag::GRAPHIC_DATA;
    /// (0070,0023) GraphicType: shared by SCOORD and SCOORD3D.
    pub const GRAPHIC_TYPE: Tag = Tag::GRAPHIC_TYPE;
    /// (3006,0024) ReferencedFrameOfReferenceUID: Type 1 for SCOORD3D.
    pub const REFERENCED_FRAME_OF_REFERENCE_UID: Tag = Tag::REFERENCED_FRAME_OF_REFERENCE_UID;
    /// (0040,A130) TemporalRangeType of a TCOORD item.
    pub const TEMPORAL_RANGE_TYPE: Tag = Tag::TEMPORAL_RANGE_TYPE;
    /// (0040,A132) ReferencedSamplePositions (UL).
    pub const REFERENCED_SAMPLE_POSITIONS: Tag = Tag::REFERENCED_SAMPLE_POSITIONS;
    /// (0040,A138) ReferencedTimeOffsets (DS).
    pub const REFERENCED_TIME_OFFSETS: Tag = Tag::REFERENCED_TIME_OFFSETS;
    /// (0040,A13A) ReferencedDateTime (DT).
    pub const REFERENCED_DATE_TIME: Tag = Tag::REFERENCED_DATE_TIME;
    /// (0040,A0B0) ReferencedWaveformChannels (US, 2-2n).
    pub const REFERENCED_WAVEFORM_CHANNELS: Tag = Tag::REFERENCED_WAVEFORM_CHANNELS;
    /// (0040,A491) CompletionFlag of the SR document.
    pub const COMPLETION_FLAG: Tag = Tag::COMPLETION_FLAG;
    /// (0040,A493) VerificationFlag of the SR document.
    pub const VERIFICATION_FLAG: Tag = Tag::VERIFICATION_FLAG;
    /// (0040,DB73) ReferencedContentItemIdentifier: the target of a by-reference
    /// relationship, which carries no ValueType (Table C.17-6).
    pub const REFERENCED_CONTENT_ITEM_IDENTIFIER: Tag = Tag::REFERENCED_CONTENT_ITEM_IDENTIFIER;
}

/// The 15 SR value types (§12.3, PS3.3 C.18.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Container,
    Text,
    Code,
    Num,
    PersonName,
    Date,
    Time,
    DateTime,
    UidRef,
    Composite,
    Image,
    Waveform,
    Scoord,
    Scoord3D,
    Tcoord,
}

impl ValueType {
    pub fn as_str(self) -> &'static str {
        match self {
            ValueType::Container => "CONTAINER",
            ValueType::Text => "TEXT",
            ValueType::Code => "CODE",
            ValueType::Num => "NUM",
            ValueType::PersonName => "PNAME",
            ValueType::Date => "DATE",
            ValueType::Time => "TIME",
            ValueType::DateTime => "DATETIME",
            ValueType::UidRef => "UIDREF",
            ValueType::Composite => "COMPOSITE",
            ValueType::Image => "IMAGE",
            ValueType::Waveform => "WAVEFORM",
            ValueType::Scoord => "SCOORD",
            ValueType::Scoord3D => "SCOORD3D",
            ValueType::Tcoord => "TCOORD",
        }
    }

    pub fn parse(text: &str) -> Option<ValueType> {
        Some(match text.trim() {
            "CONTAINER" => ValueType::Container,
            "TEXT" => ValueType::Text,
            "CODE" => ValueType::Code,
            "NUM" => ValueType::Num,
            "PNAME" => ValueType::PersonName,
            "DATE" => ValueType::Date,
            "TIME" => ValueType::Time,
            "DATETIME" => ValueType::DateTime,
            "UIDREF" => ValueType::UidRef,
            "COMPOSITE" => ValueType::Composite,
            "IMAGE" => ValueType::Image,
            "WAVEFORM" => ValueType::Waveform,
            "SCOORD" => ValueType::Scoord,
            "SCOORD3D" => ValueType::Scoord3D,
            "TCOORD" => ValueType::Tcoord,
            _ => return None,
        })
    }
}

/// The 7 relationship types (§12.3, PS3.3 C.18.1), plus root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relationship {
    Contains,
    HasProperties,
    InferredFrom,
    SelectedFrom,
    HasObsContext,
    HasAcqContext,
    HasConceptMod,
}

impl Relationship {
    pub fn as_str(self) -> &'static str {
        match self {
            Relationship::Contains => "CONTAINS",
            Relationship::HasProperties => "HAS PROPERTIES",
            Relationship::InferredFrom => "INFERRED FROM",
            Relationship::SelectedFrom => "SELECTED FROM",
            Relationship::HasObsContext => "HAS OBS CONTEXT",
            Relationship::HasAcqContext => "HAS ACQ CONTEXT",
            Relationship::HasConceptMod => "HAS CONCEPT MOD",
        }
    }

    pub fn parse(text: &str) -> Option<Relationship> {
        Some(match text.trim() {
            "CONTAINS" => Relationship::Contains,
            "HAS PROPERTIES" => Relationship::HasProperties,
            "INFERRED FROM" => Relationship::InferredFrom,
            "SELECTED FROM" => Relationship::SelectedFrom,
            "HAS OBS CONTEXT" => Relationship::HasObsContext,
            "HAS ACQ CONTEXT" => Relationship::HasAcqContext,
            "HAS CONCEPT MOD" => Relationship::HasConceptMod,
            _ => return None,
        })
    }
}

/// The 2D graphic types of a SCOORD item (PS3.3 C.18.6.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicType {
    Point,
    Multipoint,
    Polyline,
    Circle,
    Ellipse,
}

impl GraphicType {
    pub fn as_str(self) -> &'static str {
        match self {
            GraphicType::Point => "POINT",
            GraphicType::Multipoint => "MULTIPOINT",
            GraphicType::Polyline => "POLYLINE",
            GraphicType::Circle => "CIRCLE",
            GraphicType::Ellipse => "ELLIPSE",
        }
    }

    pub fn parse(text: &str) -> Option<GraphicType> {
        Some(match text.trim() {
            "POINT" => GraphicType::Point,
            "MULTIPOINT" => GraphicType::Multipoint,
            "POLYLINE" => GraphicType::Polyline,
            "CIRCLE" => GraphicType::Circle,
            "ELLIPSE" => GraphicType::Ellipse,
            _ => return None,
        })
    }

    /// How many (column,row) pairs this type requires (C.18.6.1.2).
    pub(crate) fn point_range(self) -> (usize, Option<usize>) {
        match self {
            GraphicType::Point => (1, Some(1)),
            GraphicType::Multipoint => (1, None),
            GraphicType::Polyline => (2, None),
            GraphicType::Circle => (2, Some(2)),
            GraphicType::Ellipse => (4, Some(4)),
        }
    }
}

/// The 3D graphic types of a SCOORD3D item (PS3.3 C.18.9.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicType3D {
    Point,
    Multipoint,
    Polyline,
    Polygon,
    Ellipse,
    Ellipsoid,
}

impl GraphicType3D {
    pub fn as_str(self) -> &'static str {
        match self {
            GraphicType3D::Point => "POINT",
            GraphicType3D::Multipoint => "MULTIPOINT",
            GraphicType3D::Polyline => "POLYLINE",
            GraphicType3D::Polygon => "POLYGON",
            GraphicType3D::Ellipse => "ELLIPSE",
            GraphicType3D::Ellipsoid => "ELLIPSOID",
        }
    }

    pub fn parse(text: &str) -> Option<GraphicType3D> {
        Some(match text.trim() {
            "POINT" => GraphicType3D::Point,
            "MULTIPOINT" => GraphicType3D::Multipoint,
            "POLYLINE" => GraphicType3D::Polyline,
            "POLYGON" => GraphicType3D::Polygon,
            "ELLIPSE" => GraphicType3D::Ellipse,
            "ELLIPSOID" => GraphicType3D::Ellipsoid,
            _ => return None,
        })
    }

    /// How many (x,y,z) triplets this type requires (C.18.9.1.2).
    pub(crate) fn point_range(self) -> (usize, Option<usize>) {
        match self {
            GraphicType3D::Point => (1, Some(1)),
            GraphicType3D::Multipoint => (1, None),
            GraphicType3D::Polyline => (2, None),
            GraphicType3D::Polygon => (3, None),
            GraphicType3D::Ellipse => (4, Some(4)),
            GraphicType3D::Ellipsoid => (6, Some(6)),
        }
    }
}

/// The temporal extent of a TCOORD item (PS3.3 C.18.7.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalRangeType {
    Point,
    Multipoint,
    Segment,
    Multisegment,
    Begin,
    End,
}

impl TemporalRangeType {
    pub fn as_str(self) -> &'static str {
        match self {
            TemporalRangeType::Point => "POINT",
            TemporalRangeType::Multipoint => "MULTIPOINT",
            TemporalRangeType::Segment => "SEGMENT",
            TemporalRangeType::Multisegment => "MULTISEGMENT",
            TemporalRangeType::Begin => "BEGIN",
            TemporalRangeType::End => "END",
        }
    }

    pub fn parse(text: &str) -> Option<TemporalRangeType> {
        Some(match text.trim() {
            "POINT" => TemporalRangeType::Point,
            "MULTIPOINT" => TemporalRangeType::Multipoint,
            "SEGMENT" => TemporalRangeType::Segment,
            "MULTISEGMENT" => TemporalRangeType::Multisegment,
            "BEGIN" => TemporalRangeType::Begin,
            "END" => TemporalRangeType::End,
            _ => return None,
        })
    }

    /// How many temporal points this range type requires (C.18.7.1.1).
    fn point_range(self) -> (usize, Option<usize>) {
        match self {
            TemporalRangeType::Point | TemporalRangeType::Begin | TemporalRangeType::End => {
                (1, Some(1))
            }
            TemporalRangeType::Segment => (2, Some(2)),
            TemporalRangeType::Multipoint => (1, None),
            TemporalRangeType::Multisegment => (2, None),
        }
    }
}

/// Narrows a numeric value read from a binary element to an unsigned integer.
/// A non-integral, negative or out-of-range value is corrupt data, not a value
/// to wrap.
fn unsigned<T: TryFrom<u32>>(value: f64) -> Option<T> {
    if value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= u32::MAX as f64 {
        T::try_from(value as u32).ok()
    } else {
        None
    }
}

/// Whether a coordinate survives the f32 round trip the FL writer performs. A
/// value that overflows f32, or is NaN/infinity, has no FL form.
fn fits_f32(value: f64) -> bool {
    (value as f32).is_finite()
}

/// Checks a point count against the type's requirement.
pub(crate) fn check_count(
    kind: &str,
    count: usize,
    (min, max): (usize, Option<usize>),
) -> Result<(), Error> {
    let fits = count >= min && max.is_none_or(|max| count <= max);
    if fits {
        return Ok(());
    }
    let expected = match max {
        Some(max) if max == min => format!("exactly {min}"),
        Some(max) => format!("{min} to {max}"),
        None => format!("at least {min}"),
    };
    Err(Error::Data(DataError::Other {
        message: format!("{kind} needs {expected} points, got {count}"),
    }))
}

/// A SCOORD value: the graphic type plus (column,row) pairs (PS3.3 C.18.6).
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialCoordinates {
    pub graphic_type: GraphicType,
    pub points: Vec<(f64, f64)>,
}

impl SpatialCoordinates {
    /// Validates the point count and that every coordinate is finite: FL has no
    /// form for NaN or infinity (C.18.6.1.2).
    pub fn new(graphic_type: GraphicType, points: &[(f64, f64)]) -> Result<Self, Error> {
        check_count("SCOORD", points.len(), graphic_type.point_range())?;
        // FL cannot hold a value that overflows f32, nor NaN/infinity, so reject
        // a coordinate the writer would silently turn into infinity (C.18.6.1.2)
        if points
            .iter()
            .any(|(column, row)| !fits_f32(*column) || !fits_f32(*row))
        {
            return Err(Error::Data(DataError::Other {
                message: "SCOORD coordinates must be finite".into(),
            }));
        }
        Ok(SpatialCoordinates {
            graphic_type,
            points: points.to_vec(),
        })
    }
}

/// A SCOORD3D value: the graphic type, (x,y,z) triplets and their frame of
/// reference (PS3.3 C.18.9).
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialCoordinates3D {
    pub graphic_type: GraphicType3D,
    pub points: Vec<(f64, f64, f64)>,
    /// (3006,0024) Referenced Frame of Reference UID, Type 1 in C.18.9.
    pub frame_of_reference_uid: String,
}

impl SpatialCoordinates3D {
    /// Validates the point count and, for POLYGON, that the first and last
    /// vertices are the same (C.18.9.1.2).
    pub fn new(
        graphic_type: GraphicType3D,
        points: &[(f64, f64, f64)],
        frame_of_reference_uid: impl Into<String>,
    ) -> Result<Self, Error> {
        check_count("SCOORD3D", points.len(), graphic_type.point_range())?;
        if points
            .iter()
            .any(|(x, y, z)| !fits_f32(*x) || !fits_f32(*y) || !fits_f32(*z))
        {
            return Err(Error::Data(DataError::Other {
                message: "SCOORD3D coordinates must be finite".into(),
            }));
        }
        let frame_of_reference_uid = frame_of_reference_uid.into();
        if !crate::model::uid::Uid::is_valid(&frame_of_reference_uid) {
            return Err(Error::Data(DataError::Other {
                message: "SCOORD3D needs a Referenced Frame of Reference UID".into(),
            }));
        }
        if graphic_type == GraphicType3D::Polygon && points.first() != points.last() {
            return Err(Error::Data(DataError::Other {
                message: "SCOORD3D POLYGON needs the first and last vertices to be the same".into(),
            }));
        }
        Ok(SpatialCoordinates3D {
            graphic_type,
            points: points.to_vec(),
            frame_of_reference_uid,
        })
    }
}

/// The temporal points of a TCOORD item (PS3.3 C.18.7); exactly one form.
#[derive(Debug, Clone, PartialEq)]
pub enum TemporalRange {
    /// (0040,A132) ReferencedSamplePositions, 1-based.
    SamplePositions(Vec<u32>),
    /// (0040,A138) ReferencedTimeOffsets, seconds after the start of the data.
    TimeOffsets(Vec<f64>),
    /// (0040,A13A) ReferencedDateTime.
    DateTimes(Vec<String>),
}

impl TemporalRange {
    pub fn len(&self) -> usize {
        match self {
            TemporalRange::SamplePositions(values) => values.len(),
            TemporalRange::TimeOffsets(values) => values.len(),
            TemporalRange::DateTimes(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A TCOORD value: the temporal range type plus its points (PS3.3 C.18.7).
#[derive(Debug, Clone, PartialEq)]
pub struct TemporalCoordinates {
    pub range_type: TemporalRangeType,
    pub range: TemporalRange,
}

impl TemporalCoordinates {
    /// Validates the point count, the even MULTISEGMENT count, finite time
    /// offsets and the DT values.
    pub fn new(range_type: TemporalRangeType, range: TemporalRange) -> Result<Self, Error> {
        check_count("TCOORD", range.len(), range_type.point_range())?;
        if range_type == TemporalRangeType::Multisegment && !range.len().is_multiple_of(2) {
            return Err(Error::Data(DataError::Other {
                message: format!(
                    "TCOORD MULTISEGMENT needs an even number of points, got {}",
                    range.len()
                ),
            }));
        }
        match &range {
            // DS has no form for NaN or infinity
            TemporalRange::TimeOffsets(values) => {
                if values.iter().any(|value| !value.is_finite()) {
                    return Err(Error::Data(DataError::Other {
                        message: "TCOORD time offsets must be finite".into(),
                    }));
                }
            }
            TemporalRange::DateTimes(values) => {
                for value in values {
                    crate::model::dicom_datetime::parse_dt(value)?;
                }
            }
            TemporalRange::SamplePositions(values) => {
                if values.contains(&0) {
                    return Err(Error::Data(DataError::Other {
                        message: "TCOORD sample positions are 1-based".into(),
                    }));
                }
            }
        }
        Ok(TemporalCoordinates { range_type, range })
    }
}

/// The typed value of a content item (PS3.3 C.18.1): one variant per ValueType.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentValue {
    /// A CONTAINER item; read its children with [`ContentItem::children`].
    Container,
    Text(String),
    Code(CodeItem),
    Num(MeasuredValue),
    PersonName(String),
    Date(String),
    Time(String),
    DateTime(String),
    UidRef(String),
    /// COMPOSITE / IMAGE: the referenced SOP instances.
    References(Vec<ReferencedSop>),
    /// WAVEFORM: the referenced SOP instances and their (multiplex group,
    /// channel) pairs; an empty list means every channel.
    Waveform {
        references: Vec<ReferencedSop>,
        channels: Vec<(u16, u16)>,
    },
    Scoord(SpatialCoordinates),
    Scoord3D(SpatialCoordinates3D),
    Tcoord(TemporalCoordinates),
}

/// A NUM measured value: a numeric value plus its measurement units (§12.3).
#[derive(Debug, Clone, PartialEq)]
pub struct MeasuredValue {
    pub value: f64,
    pub units: CodeItem,
}

impl MeasuredValue {
    pub fn new(value: f64, units: CodeItem) -> Self {
        MeasuredValue { value, units }
    }

    /// The DS text of [`value`](Self::value) (PS3.5 §6.2: at most 16 bytes).
    ///
    /// Uses the crate's DS formatter (the one `IntoElement for f64` uses), so
    /// the result is always a legal DS of at most 16 characters — values that
    /// need more are demoted to 10 significant digits, as fo-dicom's
    /// `ToDecimalString` does. A non-finite value has no DS form at all and is
    /// rejected rather than written.
    pub fn numeric_value_text(&self) -> Result<String, Error> {
        if !self.value.is_finite() {
            return Err(no_ds_form(self.value, "it is not a finite number"));
        }
        Ok(crate::model::person_name::format_ds(self.value))
    }

    /// Writes NumericValue (DS) and MeasurementUnitsCodeSequence into `dataset`
    /// (the item of a MeasuredValueSequence).
    pub fn write_into(&self, dataset: &mut Dataset) -> Result<(), Error> {
        let text = self.numeric_value_text()?;
        dataset.insert_str(tags::NUMERIC_VALUE, Vr::Ds, text);
        let mut units = Dataset::new();
        self.units.write_into(&mut units);
        dataset
            .insert_sequence(tags::MEASUREMENT_UNITS_CODE_SEQUENCE)
            .push(units);
        Ok(())
    }

    /// Reads a measured value from a MeasuredValueSequence item (§12.3).
    pub fn read_from(dataset: &Dataset) -> Result<Self, Error> {
        let text = dataset.try_get_str(tags::NUMERIC_VALUE).ok_or_else(|| {
            Error::Data(DataError::Other {
                message: "measured value lacks NumericValue".into(),
            })
        })?;
        let value = text.trim().parse::<f64>().map_err(|_| {
            Error::Data(DataError::Other {
                message: format!("numeric value `{text}` is not a number"),
            })
        })?;
        let units = dataset
            .try_get_sequence(tags::MEASUREMENT_UNITS_CODE_SEQUENCE)
            .and_then(|sequence| sequence.items().first())
            .and_then(|item| CodeItem::read_from(item).ok())
            .ok_or_else(|| {
                Error::Data(DataError::Other {
                    message: "measured value lacks units".into(),
                })
            })?;
        Ok(MeasuredValue { value, units })
    }
}

/// The error for a numeric value with no legal DS form.
fn no_ds_form(value: f64, why: &str) -> Error {
    Error::Data(DataError::Other {
        message: format!("numeric value {value} has no DS form within 16 characters: {why}"),
    })
}

/// A reference to a SOP instance (COMPOSITE / IMAGE content items, §12.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferencedSop {
    pub class_uid: String,
    pub instance_uid: String,
}

impl ReferencedSop {
    pub fn new(class_uid: impl Into<String>, instance_uid: impl Into<String>) -> Self {
        ReferencedSop {
            class_uid: class_uid.into(),
            instance_uid: instance_uid.into(),
        }
    }

    /// Writes ReferencedSOPClassUID / ReferencedSOPInstanceUID into `dataset`
    /// (an item of a ReferencedSOPSequence).
    pub fn write_into(&self, dataset: &mut Dataset) {
        dataset.insert_str(
            tags::REFERENCED_SOP_CLASS_UID,
            Vr::Ui,
            self.class_uid.as_str(),
        );
        dataset.insert_str(
            tags::REFERENCED_SOP_INSTANCE_UID,
            Vr::Ui,
            self.instance_uid.as_str(),
        );
    }

    /// Reads a reference from a ReferencedSOPSequence item (§12.3).
    pub fn read_from(dataset: &Dataset) -> Result<Self, Error> {
        let class_uid = dataset
            .try_get_str(tags::REFERENCED_SOP_CLASS_UID)
            .ok_or_else(|| {
                Error::Data(DataError::Other {
                    message: "reference lacks ReferencedSOPClassUID".into(),
                })
            })?;
        let instance_uid = dataset
            .try_get_str(tags::REFERENCED_SOP_INSTANCE_UID)
            .ok_or_else(|| {
                Error::Data(DataError::Other {
                    message: "reference lacks ReferencedSOPInstanceUID".into(),
                })
            })?;
        Ok(ReferencedSop {
            class_uid: class_uid.trim().to_string(),
            instance_uid: instance_uid.trim().to_string(),
        })
    }
}

/// A waveform reference and its channel selection (empty means all channels).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveformReference {
    pub reference: ReferencedSop,
    pub channels: Vec<(u16, u16)>,
}

/// One content item: the tagged dataset plus its typed value (§12.3).
#[derive(Debug, Clone)]
pub struct ContentItem {
    dataset: Dataset,
}

impl ContentItem {
    /// Starts a container with a concept name (§12.3).
    ///
    /// ContinuityOfContent is Type 1 for a CONTAINER, so an absent value is
    /// written as `SEPARATE` (fo-dicom's default) rather than left missing.
    pub fn container(
        concept_name: CodeItem,
        relationship: Option<Relationship>,
        continuity: Option<&str>,
    ) -> Self {
        let mut item = Self::base(concept_name, ValueType::Container, relationship);
        item.dataset.insert_str(
            tags::CONTINUITY_OF_CONTENT,
            Vr::Cs,
            continuity.unwrap_or("SEPARATE"),
        );
        item
    }

    /// A TEXT value (§12.3).
    pub fn text(
        concept_name: CodeItem,
        relationship: Relationship,
        text: impl Into<String>,
    ) -> Self {
        let mut item = Self::base(concept_name, ValueType::Text, Some(relationship));
        item.dataset.insert_str(tags::TEXT_VALUE, Vr::Ut, text);
        item
    }

    /// A CODE value: the code is written as a ConceptCodeSequence item (§12.3).
    pub fn code(concept_name: CodeItem, relationship: Relationship, code: CodeItem) -> Self {
        let mut item = Self::base(concept_name, ValueType::Code, Some(relationship));
        let mut coded = Dataset::new();
        code.write_into(&mut coded);
        item.dataset
            .insert_sequence(tags::CONCEPT_CODE_SEQUENCE)
            .push(coded);
        item
    }

    /// A PNAME value (§12.3).
    pub fn person_name(
        concept_name: CodeItem,
        relationship: Relationship,
        name: impl Into<String>,
    ) -> Self {
        let mut item = Self::base(concept_name, ValueType::PersonName, Some(relationship));
        item.dataset.insert_str(tags::PERSON_NAME, Vr::Pn, name);
        item
    }

    /// A DATETIME value (§12.3).
    pub fn date_time(
        concept_name: CodeItem,
        relationship: Relationship,
        value: impl Into<String>,
    ) -> Self {
        let mut item = Self::base(concept_name, ValueType::DateTime, Some(relationship));
        item.dataset.insert_str(tags::DATE_TIME, Vr::Dt, value);
        item
    }

    /// A DATE value (§12.3).
    pub fn date(
        concept_name: CodeItem,
        relationship: Relationship,
        value: impl Into<String>,
    ) -> Self {
        let mut item = Self::base(concept_name, ValueType::Date, Some(relationship));
        item.dataset.insert_str(tags::DATE, Vr::Da, value);
        item
    }

    /// A TIME value (§12.3).
    pub fn time(
        concept_name: CodeItem,
        relationship: Relationship,
        value: impl Into<String>,
    ) -> Self {
        let mut item = Self::base(concept_name, ValueType::Time, Some(relationship));
        item.dataset.insert_str(tags::TIME, Vr::Tm, value);
        item
    }

    /// A UIDREF value (§12.3).
    pub fn uid_ref(
        concept_name: CodeItem,
        relationship: Relationship,
        uid: impl Into<String>,
    ) -> Self {
        let mut item = Self::base(concept_name, ValueType::UidRef, Some(relationship));
        item.dataset.insert_str(tags::UID, Vr::Ui, uid);
        item
    }

    /// A NUM value: one MeasuredValueSequence item (§12.3).
    ///
    /// Fails when the value has no legal DS form (see
    /// [`MeasuredValue::numeric_value_text`]).
    pub fn num(
        concept_name: CodeItem,
        relationship: Relationship,
        measured: MeasuredValue,
    ) -> Result<Self, Error> {
        let mut item = Self::base(concept_name, ValueType::Num, Some(relationship));
        let mut measured_item = Dataset::new();
        measured.write_into(&mut measured_item)?;
        item.dataset
            .insert_sequence(tags::MEASURED_VALUE_SEQUENCE)
            .push(measured_item);
        Ok(item)
    }

    /// A COMPOSITE/IMAGE value: ReferencedSOPSequence items (§12.3).
    pub fn referenced_sop(
        concept_name: CodeItem,
        relationship: Relationship,
        value_type: ValueType,
        references: &[ReferencedSop],
    ) -> Self {
        let mut item = Self::base(concept_name, value_type, Some(relationship));
        let sequence = item.dataset.insert_sequence(tags::REFERENCED_SOP_SEQUENCE);
        for reference in references {
            let mut reference_item = Dataset::new();
            reference.write_into(&mut reference_item);
            sequence.push(reference_item);
        }
        item
    }

    /// The backing dataset, for reading value tags.
    pub fn dataset(&self) -> &Dataset {
        &self.dataset
    }

    /// A SCOORD value: GraphicType and GraphicData (PS3.3 C.18.6). The point
    /// count is checked by [`SpatialCoordinates::new`].
    pub fn scoord(
        concept_name: CodeItem,
        relationship: Relationship,
        value: SpatialCoordinates,
    ) -> Self {
        let mut item = Self::base(concept_name, ValueType::Scoord, Some(relationship));
        item.dataset
            .insert_str(tags::GRAPHIC_TYPE, Vr::Cs, value.graphic_type.as_str());
        let mut bytes = Vec::with_capacity(value.points.len() * 8);
        for (column, row) in &value.points {
            bytes.extend_from_slice(&(*column as f32).to_le_bytes());
            bytes.extend_from_slice(&(*row as f32).to_le_bytes());
        }
        insert_bytes(&mut item.dataset, tags::GRAPHIC_DATA, Vr::Fl, bytes);
        item
    }

    /// A SCOORD3D value: GraphicType, GraphicData triplets and the referenced
    /// frame of reference (PS3.3 C.18.9).
    pub fn scoord3d(
        concept_name: CodeItem,
        relationship: Relationship,
        value: SpatialCoordinates3D,
    ) -> Self {
        let mut item = Self::base(concept_name, ValueType::Scoord3D, Some(relationship));
        item.dataset
            .insert_str(tags::GRAPHIC_TYPE, Vr::Cs, value.graphic_type.as_str());
        item.dataset.insert_str(
            tags::REFERENCED_FRAME_OF_REFERENCE_UID,
            Vr::Ui,
            value.frame_of_reference_uid.as_str(),
        );
        let mut bytes = Vec::with_capacity(value.points.len() * 12);
        for (x, y, z) in &value.points {
            bytes.extend_from_slice(&(*x as f32).to_le_bytes());
            bytes.extend_from_slice(&(*y as f32).to_le_bytes());
            bytes.extend_from_slice(&(*z as f32).to_le_bytes());
        }
        insert_bytes(&mut item.dataset, tags::GRAPHIC_DATA, Vr::Fl, bytes);
        item
    }

    /// A TCOORD value: TemporalRangeType plus one of the three reference lists
    /// (PS3.3 C.18.7).
    pub fn tcoord(
        concept_name: CodeItem,
        relationship: Relationship,
        value: TemporalCoordinates,
    ) -> Self {
        let mut item = Self::base(concept_name, ValueType::Tcoord, Some(relationship));
        item.dataset
            .insert_str(tags::TEMPORAL_RANGE_TYPE, Vr::Cs, value.range_type.as_str());
        match &value.range {
            TemporalRange::SamplePositions(values) => {
                let mut bytes = Vec::with_capacity(values.len() * 4);
                for value in values {
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
                insert_bytes(
                    &mut item.dataset,
                    tags::REFERENCED_SAMPLE_POSITIONS,
                    Vr::Ul,
                    bytes,
                );
            }
            TemporalRange::TimeOffsets(values) => {
                let text = values
                    .iter()
                    .map(|value| crate::model::person_name::format_ds(*value))
                    .collect::<Vec<_>>()
                    .join("\\");
                item.dataset
                    .insert_str(tags::REFERENCED_TIME_OFFSETS, Vr::Ds, text);
            }
            TemporalRange::DateTimes(values) => {
                item.dataset
                    .insert_str(tags::REFERENCED_DATE_TIME, Vr::Dt, values.join("\\"));
            }
        }
        item
    }

    /// A WAVEFORM value: the composite object reference plus the optional
    /// (multiplex group, channel) pairs (PS3.3 C.18.5).
    pub fn waveform(
        concept_name: CodeItem,
        relationship: Relationship,
        references: &[ReferencedSop],
        channels: &[(u16, u16)],
    ) -> Self {
        let mut item = Self::base(concept_name, ValueType::Waveform, Some(relationship));
        let sequence = item.dataset.insert_sequence(tags::REFERENCED_SOP_SEQUENCE);
        for reference in references {
            let mut reference_item = Dataset::new();
            reference.write_into(&mut reference_item);
            if !channels.is_empty() {
                let mut bytes = Vec::with_capacity(channels.len() * 4);
                for (group, channel) in channels {
                    bytes.extend_from_slice(&group.to_le_bytes());
                    bytes.extend_from_slice(&channel.to_le_bytes());
                }
                insert_bytes(
                    &mut reference_item,
                    tags::REFERENCED_WAVEFORM_CHANNELS,
                    Vr::Us,
                    bytes,
                );
            }
            sequence.push(reference_item);
        }
        item
    }

    /// Consumes the item, yielding its backing dataset.
    pub fn into_dataset(self) -> Dataset {
        self.dataset
    }

    /// Wraps an existing dataset as a content item (used when reading a tree).
    pub fn from_dataset(dataset: Dataset) -> Self {
        ContentItem { dataset }
    }

    pub fn value_type(&self) -> Option<ValueType> {
        self.dataset
            .try_get_str(tags::VALUE_TYPE)
            .and_then(|s| ValueType::parse(&s))
    }

    pub fn relationship(&self) -> Option<Relationship> {
        self.dataset
            .try_get_str(tags::RELATIONSHIP_TYPE)
            .and_then(|s| Relationship::parse(&s))
    }

    /// The concept name code, when present.
    pub fn concept_name(&self) -> Option<CodeItem> {
        let sequence = self.dataset.try_get_sequence(tags::CONCEPT_NAME)?;
        sequence
            .items()
            .first()
            .and_then(|item| CodeItem::read_from(item).ok())
    }

    /// The text value, if this is a TEXT item.
    pub fn text_value(&self) -> Option<String> {
        self.dataset.try_get_str(tags::TEXT_VALUE)
    }

    /// The measured value, if this is a NUM item (§12.3).
    pub fn measured_value(&self) -> Option<MeasuredValue> {
        self.dataset
            .try_get_sequence(tags::MEASURED_VALUE_SEQUENCE)
            .and_then(|sequence| sequence.items().first())
            .and_then(|item| MeasuredValue::read_from(item).ok())
    }

    /// The referenced SOP instances of a COMPOSITE/IMAGE item (§12.3).
    pub fn referenced_sops(&self) -> Vec<ReferencedSop> {
        self.dataset
            .try_get_sequence(tags::REFERENCED_SOP_SEQUENCE)
            .map(|sequence| {
                sequence
                    .items()
                    .iter()
                    .filter_map(|item| ReferencedSop::read_from(item).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The code value of a CODE item: ConceptCodeSequence (§12.3).
    pub fn code_value(&self) -> Option<CodeItem> {
        let sequence = self.dataset.try_get_sequence(tags::CONCEPT_CODE_SEQUENCE)?;
        sequence
            .items()
            .first()
            .and_then(|item| CodeItem::read_from(item).ok())
    }

    /// The PersonName of a PNAME item.
    pub fn person_name_value(&self) -> Option<String> {
        self.dataset.try_get_str(tags::PERSON_NAME)
    }

    /// The Date of a DATE item.
    pub fn date_value(&self) -> Option<String> {
        self.dataset.try_get_str(tags::DATE)
    }

    /// The Time of a TIME item.
    pub fn time_value(&self) -> Option<String> {
        self.dataset.try_get_str(tags::TIME)
    }

    /// The DateTime of a DATETIME item.
    pub fn date_time_value(&self) -> Option<String> {
        self.dataset.try_get_str(tags::DATE_TIME)
    }

    /// The UID of a UIDREF item.
    pub fn uid_value(&self) -> Option<String> {
        self.dataset.try_get_str(tags::UID)
    }

    /// The SCOORD value of a SCOORD item (PS3.3 C.18.6).
    pub fn scoord_value(&self) -> Option<SpatialCoordinates> {
        let graphic_type = GraphicType::parse(&self.dataset.try_get_str(tags::GRAPHIC_TYPE)?)?;
        let values = self.numbers(tags::GRAPHIC_DATA)?;
        // a trailing half-pair is corrupt data: refusing it beats silently
        // dropping the last coordinate
        if !values.len().is_multiple_of(2) {
            return None;
        }
        Some(SpatialCoordinates {
            graphic_type,
            points: values
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| (pair[0], pair[1]))
                .collect(),
        })
    }

    /// The SCOORD3D value of a SCOORD3D item (PS3.3 C.18.9).
    pub fn scoord3d_value(&self) -> Option<SpatialCoordinates3D> {
        let graphic_type = GraphicType3D::parse(&self.dataset.try_get_str(tags::GRAPHIC_TYPE)?)?;
        let values = self.numbers(tags::GRAPHIC_DATA)?;
        // a trailing partial triplet is corrupt data: refuse it rather than
        // dropping the last point
        if !values.len().is_multiple_of(3) {
            return None;
        }
        Some(SpatialCoordinates3D {
            graphic_type,
            points: values
                .as_chunks::<3>()
                .0
                .iter()
                .map(|triplet| (triplet[0], triplet[1], triplet[2]))
                .collect(),
            frame_of_reference_uid: self
                .dataset
                .try_get_str(tags::REFERENCED_FRAME_OF_REFERENCE_UID)?,
        })
    }

    /// The TCOORD value of a TCOORD item (PS3.3 C.18.7).
    ///
    /// Exactly one of the three reference lists is required, so an item carrying
    /// two of them is malformed rather than ambiguously readable.
    pub fn tcoord_value(&self) -> Option<TemporalCoordinates> {
        let range_type =
            TemporalRangeType::parse(&self.dataset.try_get_str(tags::TEMPORAL_RANGE_TYPE)?)?;
        let present = [
            tags::REFERENCED_SAMPLE_POSITIONS,
            tags::REFERENCED_TIME_OFFSETS,
            tags::REFERENCED_DATE_TIME,
        ]
        .into_iter()
        .filter(|tag| self.dataset.contains(tag))
        .count();
        if present != 1 {
            return None;
        }
        let range = if self.dataset.contains(&tags::REFERENCED_SAMPLE_POSITIONS) {
            TemporalRange::SamplePositions(
                self.numbers(tags::REFERENCED_SAMPLE_POSITIONS)?
                    .into_iter()
                    .map(unsigned::<u32>)
                    .collect::<Option<Vec<u32>>>()?,
            )
        } else if let Some(text) = self.dataset.try_get_str(tags::REFERENCED_TIME_OFFSETS) {
            let mut offsets = Vec::new();
            for part in text.split('\\') {
                offsets.push(crate::model::person_name::parse_ds(part).ok()?);
            }
            TemporalRange::TimeOffsets(offsets)
        } else {
            let text = self.dataset.try_get_str(tags::REFERENCED_DATE_TIME)?;
            TemporalRange::DateTimes(text.split('\\').map(str::to_string).collect())
        };
        Some(TemporalCoordinates { range_type, range })
    }

    /// Channel selections grouped by their referenced SOP, preserving their identity.
    /// `None` means a malformed reference or channel list.
    pub fn waveform_references(&self) -> Option<Vec<WaveformReference>> {
        let sequence = self
            .dataset
            .try_get_sequence(tags::REFERENCED_SOP_SEQUENCE)?;
        sequence
            .items()
            .iter()
            .map(|dataset| {
                let reference = ReferencedSop::read_from(dataset).ok()?;
                let mut channels = Vec::new();
                if dataset.contains(&tags::REFERENCED_WAVEFORM_CHANNELS) {
                    let element = dataset
                        .get_element(tags::REFERENCED_WAVEFORM_CHANNELS)
                        .ok()?;
                    if element.vr() != Vr::Us {
                        return None;
                    }
                    if let crate::model::element::ElementValue::Bytes(buffer) = element.value() {
                        if !buffer.size().is_multiple_of(4) {
                            return None;
                        }
                    }
                    let count = dataset
                        .value_count(&tags::REFERENCED_WAVEFORM_CHANNELS)
                        .ok()?;
                    if count == 0 || !count.is_multiple_of(2) {
                        return None;
                    }
                    for index in (0..count).step_by(2) {
                        let group = dataset
                            .get_at::<u16>(&tags::REFERENCED_WAVEFORM_CHANNELS, index)
                            .ok()?;
                        let channel = dataset
                            .get_at::<u16>(&tags::REFERENCED_WAVEFORM_CHANNELS, index + 1)
                            .ok()?;
                        if group == 0 {
                            return None;
                        }
                        channels.push((group, channel));
                    }
                }
                Some(WaveformReference {
                    reference,
                    channels,
                })
            })
            .collect()
    }

    /// All channel pairs flattened across references. Use [`Self::waveform_references`]
    /// when a content item contains references with distinct channel selections.
    pub fn waveform_channels(&self) -> Vec<(u16, u16)> {
        self.waveform_references()
            .unwrap_or_default()
            .into_iter()
            .flat_map(|reference| reference.channels)
            .collect()
    }

    /// The item's typed value (PS3.3 C.18.1); `None` when the value tag is
    /// missing or malformed.
    pub fn value(&self) -> Option<ContentValue> {
        Some(match self.value_type()? {
            ValueType::Container => ContentValue::Container,
            ValueType::Text => ContentValue::Text(self.text_value()?),
            ValueType::Code => ContentValue::Code(self.code_value()?),
            ValueType::Num => ContentValue::Num(self.measured_value()?),
            ValueType::PersonName => ContentValue::PersonName(self.person_name_value()?),
            ValueType::Date => ContentValue::Date(self.date_value()?),
            ValueType::Time => ContentValue::Time(self.time_value()?),
            ValueType::DateTime => ContentValue::DateTime(self.date_time_value()?),
            ValueType::UidRef => ContentValue::UidRef(self.uid_value()?),
            ValueType::Composite | ValueType::Image => {
                ContentValue::References(self.referenced_sops())
            }
            ValueType::Waveform => {
                let references = self.waveform_references()?;
                let channels = references.first()?.channels.clone();
                if references
                    .iter()
                    .any(|reference| reference.channels != channels)
                {
                    return None;
                }
                ContentValue::Waveform {
                    references: references
                        .into_iter()
                        .map(|reference| reference.reference)
                        .collect(),
                    channels,
                }
            }
            ValueType::Scoord => ContentValue::Scoord(self.scoord_value()?),
            ValueType::Scoord3D => ContentValue::Scoord3D(self.scoord3d_value()?),
            ValueType::Tcoord => ContentValue::Tcoord(self.tcoord_value()?),
        })
    }

    /// Whether the item carries the value its ValueType requires (§12.3).
    ///
    /// A CONTAINER needs no value tag (children are optional), but every
    /// sequence-valued item needs a non-empty sequence, since an empty one
    /// carries nothing.
    pub(crate) fn has_value(&self, value_type: ValueType) -> bool {
        let has = |tag: crate::model::tag::Tag| self.dataset.contains(&tag);
        let has_items = |tag: crate::model::tag::Tag| {
            self.dataset.value_count(&tag).is_ok_and(|count| count > 0)
        };
        match value_type {
            ValueType::Container => true,
            ValueType::Text => has(tags::TEXT_VALUE),
            ValueType::Code => has_items(tags::CONCEPT_CODE_SEQUENCE),
            // Table C.18.1-1 makes MeasuredValueSequence Type 2: present, but
            // legitimately empty when the value is unknown or a failure
            ValueType::Num => has(tags::MEASURED_VALUE_SEQUENCE),
            ValueType::PersonName => has(tags::PERSON_NAME),
            ValueType::Date => has(tags::DATE),
            ValueType::Time => has(tags::TIME),
            ValueType::DateTime => has(tags::DATE_TIME),
            ValueType::UidRef => has(tags::UID),
            ValueType::Composite | ValueType::Image | ValueType::Waveform => {
                has_items(tags::REFERENCED_SOP_SEQUENCE)
            }
            ValueType::Scoord => has(tags::GRAPHIC_DATA) && has(tags::GRAPHIC_TYPE),
            ValueType::Scoord3D => {
                has(tags::GRAPHIC_DATA)
                    && has(tags::GRAPHIC_TYPE)
                    && has(tags::REFERENCED_FRAME_OF_REFERENCE_UID)
            }
            ValueType::Tcoord => {
                let lists = [
                    tags::REFERENCED_SAMPLE_POSITIONS,
                    tags::REFERENCED_TIME_OFFSETS,
                    tags::REFERENCED_DATE_TIME,
                ]
                .into_iter()
                .filter(|tag| self.dataset.contains(tag))
                .count();
                has(tags::TEMPORAL_RANGE_TYPE) && lists == 1
            }
        }
    }

    /// Every value of a numeric element as `f64` (FL/UL/US/...).
    fn numbers(&self, tag: crate::model::tag::Tag) -> Option<Vec<f64>> {
        let count = self.dataset.value_count(&tag).ok()?;
        let mut values = Vec::with_capacity(count);
        for index in 0..count {
            values.push(self.dataset.get_at::<f64>(&tag, index).ok()?);
        }
        Some(values)
    }

    /// The children of a container, read from ContentSequence (§12.3).
    pub fn children(&self) -> Vec<ContentItem> {
        self.dataset
            .try_get_sequence(tags::CONTENT_SEQUENCE)
            .map(|sequence| {
                sequence
                    .items()
                    .iter()
                    .map(|item| ContentItem {
                        dataset: item.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Appends a child item to a container's ContentSequence (§12.3).
    ///
    /// Pushes into the existing sequence item in place (O(1)); reusing the
    /// stored sequence keeps the earlier children. The old code cloned every
    /// child into a fresh sequence on each append (O(N²)).
    pub fn add_child(&mut self, child: ContentItem) -> &mut Self {
        match self.dataset.sequence_mut(tags::CONTENT_SEQUENCE) {
            Some(sequence) => sequence.push(child.dataset),
            None => {
                self.dataset
                    .insert_sequence(tags::CONTENT_SEQUENCE)
                    .push(child.dataset);
            }
        }
        self
    }

    /// The shared header: concept name, value type and relationship (§12.3).
    fn base(
        concept_name: CodeItem,
        value_type: ValueType,
        relationship: Option<Relationship>,
    ) -> Self {
        let mut dataset = Dataset::new();
        dataset.insert_str(tags::VALUE_TYPE, Vr::Cs, value_type.as_str());
        if let Some(relationship) = relationship {
            dataset.insert_str(tags::RELATIONSHIP_TYPE, Vr::Cs, relationship.as_str());
        }
        let mut concept = Dataset::new();
        concept_name.write_into(&mut concept);
        dataset.insert_sequence(tags::CONCEPT_NAME).push(concept);
        ContentItem { dataset }
    }
}

/// Writes a binary VR value from little-endian bytes (the in-memory order the
/// writer swaps from, see `IntoElement` for the numeric types).
fn insert_bytes(dataset: &mut Dataset, tag: crate::model::tag::Tag, vr: Vr, bytes: Vec<u8>) {
    dataset.insert_or_update(crate::model::item::DicomItem::Element(
        crate::model::element::DicomElement::new_bytes(
            tag,
            vr,
            std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes)),
        ),
    ));
}

/// Builds a ContentItem from an existing dataset, checking its value type.
pub fn content_item_from_dataset(dataset: Dataset) -> Result<ContentItem, Error> {
    let item = ContentItem { dataset };
    match item.value_type() {
        Some(_) => Ok(item),
        None => Err(Error::Data(DataError::Other {
            message: "content item lacks a valid ValueType".into(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coordinate_beyond_f32_has_no_fl_form() {
        // FL would store 1e40 as infinity, so construction rejects it
        assert!(SpatialCoordinates::new(GraphicType::Point, &[(1e40, 0.0)]).is_err());
        assert!(SpatialCoordinates::new(GraphicType::Point, &[(f64::NAN, 0.0)]).is_err());
        assert!(
            SpatialCoordinates3D::new(GraphicType3D::Point, &[(0.0, 0.0, 1e40)], "1.2.3").is_err()
        );
        // an ordinary value is fine
        assert!(SpatialCoordinates::new(GraphicType::Point, &[(1.0, 2.0)]).is_ok());
    }

    #[test]
    fn truncated_graphic_data_reads_as_none_instead_of_dropping_a_point() {
        // a 2D value needs pairs: one trailing float is corrupt, not a point
        let item = ContentItem::scoord(
            CodeItem::new("111030", "DCM"),
            Relationship::Contains,
            SpatialCoordinates::new(GraphicType::Point, &[(1.0, 2.0)]).unwrap(),
        );
        let mut dataset = item.into_dataset();
        insert_bytes(&mut dataset, tags::GRAPHIC_DATA, Vr::Fl, vec![0u8; 4]);
        assert!(
            ContentItem::from_dataset(dataset).scoord_value().is_none(),
            "a half pair is rejected, not silently truncated"
        );

        // a 3D value needs triplets: two trailing floats are corrupt too
        let item = ContentItem::scoord3d(
            CodeItem::new("111030", "DCM"),
            Relationship::Contains,
            SpatialCoordinates3D::new(GraphicType3D::Point, &[(1.0, 2.0, 3.0)], "1.2.3").unwrap(),
        );
        let mut dataset = item.into_dataset();
        insert_bytes(&mut dataset, tags::GRAPHIC_DATA, Vr::Fl, vec![0u8; 8]);
        assert!(ContentItem::from_dataset(dataset)
            .scoord3d_value()
            .is_none());
    }

    #[test]
    fn an_odd_waveform_channel_list_reads_as_empty() {
        let mut item = ContentItem::waveform(
            CodeItem::new("111030", "DCM"),
            Relationship::Contains,
            &[],
            &[(1, 2)],
        );
        let mut dataset = item.clone().into_dataset();
        // one US value cannot be a (group, channel) pair
        insert_bytes(
            &mut dataset,
            tags::REFERENCED_WAVEFORM_CHANNELS,
            Vr::Us,
            vec![0u8; 2],
        );
        item = ContentItem::from_dataset(dataset);
        assert!(item.waveform_channels().is_empty());
    }

    fn concept() -> CodeItem {
        CodeItem::new("121071", "DCM").with_meaning("Finding")
    }

    #[test]
    fn a_text_item_carries_its_value_and_relationship() {
        let item = ContentItem::text(concept(), Relationship::Contains, "Mass");
        assert_eq!(item.value_type(), Some(ValueType::Text));
        assert_eq!(item.relationship(), Some(Relationship::Contains));
        assert_eq!(item.text_value().as_deref(), Some("Mass"));
        assert_eq!(item.concept_name(), Some(concept()));
    }

    #[test]
    fn a_num_item_carries_a_measured_value_and_units() {
        let units = CodeItem::new("mm", "UCUM").with_meaning("millimeter");
        let item = ContentItem::num(
            concept(),
            Relationship::Contains,
            MeasuredValue::new(12.5, units.clone()),
        )
        .unwrap();
        assert_eq!(item.value_type(), Some(ValueType::Num));
        let measured = item.measured_value().expect("a measured value round-trips");
        assert_eq!(measured.value, 12.5);
        assert_eq!(measured.units, units);
    }

    #[test]
    fn a_measured_value_writes_a_legal_ds_and_rejects_non_finite() {
        let units = CodeItem::new("mm", "UCUM");
        // 0.1 + 0.2 needs 19 characters to round-trip; the DS formatter demotes
        // it to 10 significant digits instead of writing an illegal length
        let awkward = MeasuredValue::new(0.1 + 0.2, units.clone());
        let text = awkward.numeric_value_text().unwrap();
        assert!(text.len() <= 16, "\"{text}\" must fit a DS");
        assert!(
            (crate::model::person_name::parse_ds(&text).unwrap() - 0.3).abs() < 1e-9,
            "the demoted text still reads as 0.3, got {text}"
        );
        let item = ContentItem::num(concept(), Relationship::Contains, awkward).unwrap();
        assert!((item.measured_value().unwrap().value - 0.3).abs() < 1e-9);

        // a value that fits keeps its exact shortest text
        let measured = MeasuredValue::new(12.5, units.clone());
        assert_eq!(measured.numeric_value_text().unwrap(), "12.5");
        let mut ds = Dataset::new();
        measured.write_into(&mut ds).unwrap();
        assert_eq!(ds.try_get_str(tags::NUMERIC_VALUE).as_deref(), Some("12.5"));

        // a large magnitude falls back to the exponent form DS allows
        let large = MeasuredValue::new(1e300, units.clone())
            .numeric_value_text()
            .unwrap();
        assert!(large.len() <= 16 && large.contains('e'), "got {large}");

        // a non-finite value has no DS form at all
        assert!(MeasuredValue::new(f64::NAN, units)
            .numeric_value_text()
            .is_err());
    }

    #[test]
    fn a_referenced_sop_item_carries_its_references() {
        let references = vec![
            ReferencedSop::new("1.2.840.10008.5.1.4.1.1.2", "1.2.3.4"),
            ReferencedSop::new("1.2.840.10008.5.1.4.1.1.2", "1.2.3.5"),
        ];
        let item = ContentItem::referenced_sop(
            concept(),
            Relationship::Contains,
            ValueType::Image,
            &references,
        );
        assert_eq!(item.value_type(), Some(ValueType::Image));
        assert_eq!(item.referenced_sops(), references);
    }

    #[test]
    fn value_types_round_trip() {
        for value_type in [
            ValueType::Container,
            ValueType::Text,
            ValueType::Code,
            ValueType::Num,
            ValueType::PersonName,
            ValueType::Date,
            ValueType::Time,
            ValueType::DateTime,
            ValueType::UidRef,
            ValueType::Composite,
            ValueType::Image,
            ValueType::Waveform,
            ValueType::Scoord,
            ValueType::Scoord3D,
            ValueType::Tcoord,
        ] {
            assert_eq!(ValueType::parse(value_type.as_str()), Some(value_type));
        }
        assert_eq!(ValueType::parse("nonsense"), None);
    }

    #[test]
    fn relationships_round_trip() {
        for relationship in [
            Relationship::Contains,
            Relationship::HasProperties,
            Relationship::InferredFrom,
            Relationship::SelectedFrom,
            Relationship::HasObsContext,
            Relationship::HasAcqContext,
            Relationship::HasConceptMod,
        ] {
            assert_eq!(
                Relationship::parse(relationship.as_str()),
                Some(relationship)
            );
        }
    }

    #[test]
    fn a_container_holds_children_in_its_content_sequence() {
        let mut root = ContentItem::container(concept(), None, Some("SEPARATE"));
        root.add_child(ContentItem::text(
            concept(),
            Relationship::Contains,
            "child 1",
        ));
        root.add_child(ContentItem::text(
            concept(),
            Relationship::Contains,
            "child 2",
        ));

        let children = root.children();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].text_value().as_deref(), Some("child 1"));
        assert_eq!(children[1].text_value().as_deref(), Some("child 2"));
    }

    #[test]
    fn a_root_container_has_no_relationship() {
        let root = ContentItem::container(concept(), None, None);
        assert_eq!(root.relationship(), None);
        assert_eq!(root.value_type(), Some(ValueType::Container));
    }

    #[test]
    fn a_code_item_writes_into_concept_code_sequence() {
        let item = ContentItem::code(
            concept(),
            Relationship::Contains,
            CodeItem::new("T-D0050", "SRT"),
        );
        assert_eq!(item.value_type(), Some(ValueType::Code));
        let sequence = item
            .dataset()
            .try_get_sequence(tags::CONCEPT_CODE_SEQUENCE)
            .unwrap();
        assert_eq!(
            CodeItem::read_from(&sequence.items()[0]).unwrap().value,
            "T-D0050"
        );
    }

    #[test]
    fn typed_scalar_items_write_their_value_tag() {
        let date = ContentItem::date(concept(), Relationship::HasObsContext, "20260101");
        assert_eq!(
            date.dataset().try_get_str(tags::DATE).as_deref(),
            Some("20260101")
        );
        let dt = ContentItem::date_time(concept(), Relationship::HasObsContext, "20260101120000");
        assert_eq!(
            dt.dataset().try_get_str(tags::DATE_TIME).as_deref(),
            Some("20260101120000")
        );
        let uid = ContentItem::uid_ref(concept(), Relationship::InferredFrom, "1.2.3");
        assert_eq!(
            uid.dataset().try_get_str(tags::UID).as_deref(),
            Some("1.2.3")
        );
    }

    #[test]
    fn building_from_a_dataset_without_a_value_type_errors() {
        assert!(content_item_from_dataset(Dataset::new()).is_err());
    }

    #[test]
    fn relationship_parse_rejects_unknown_phrases() {
        assert_eq!(
            Relationship::parse("CONTAINS"),
            Some(Relationship::Contains)
        );
        assert_eq!(
            Relationship::parse(" HAS CONCEPT MOD "),
            Some(Relationship::HasConceptMod)
        );
        assert_eq!(Relationship::parse("BOGUS"), None);
    }

    #[test]
    fn measured_value_read_reports_missing_or_bad_parts() {
        // no NumericValue at all
        assert!(MeasuredValue::read_from(&Dataset::new()).is_err());

        // NumericValue that is not a number
        let mut ds = Dataset::new();
        ds.insert_str(tags::NUMERIC_VALUE, Vr::Ds, "abc");
        assert!(MeasuredValue::read_from(&ds).is_err());

        // a number without units
        let mut ds = Dataset::new();
        ds.insert_str(tags::NUMERIC_VALUE, Vr::Ds, "12.5");
        assert!(MeasuredValue::read_from(&ds).is_err());
    }

    #[test]
    fn referenced_sop_read_reports_missing_uids() {
        assert!(ReferencedSop::read_from(&Dataset::new()).is_err());

        let mut ds = Dataset::new();
        ds.insert_str(tags::REFERENCED_SOP_CLASS_UID, Vr::Ui, "1.2.3");
        assert!(
            ReferencedSop::read_from(&ds).is_err(),
            "instance UID missing"
        );

        ds.insert_str(tags::REFERENCED_SOP_INSTANCE_UID, Vr::Ui, " 1.2.4 ");
        let sop = ReferencedSop::read_from(&ds).unwrap();
        assert_eq!(sop.class_uid, "1.2.3");
        assert_eq!(sop.instance_uid, "1.2.4", "surrounding space is trimmed");
    }

    #[test]
    fn typed_value_builders_set_the_matching_vr() {
        let concept = concept();
        let person = ContentItem::person_name(concept.clone(), Relationship::Contains, "Doe^J");
        assert_eq!(person.value_type(), Some(ValueType::PersonName));
        assert_eq!(
            person.dataset().try_get_str(tags::PERSON_NAME).as_deref(),
            Some("Doe^J")
        );

        let date_time =
            ContentItem::date_time(concept.clone(), Relationship::Contains, "20200101120000");
        assert_eq!(date_time.value_type(), Some(ValueType::DateTime));
        assert_eq!(
            date_time.dataset().try_get_str(tags::DATE_TIME).as_deref(),
            Some("20200101120000")
        );

        let date = ContentItem::date(concept.clone(), Relationship::Contains, "20200101");
        assert_eq!(date.value_type(), Some(ValueType::Date));
        assert_eq!(
            date.dataset().try_get_str(tags::DATE).as_deref(),
            Some("20200101")
        );

        let time = ContentItem::time(concept.clone(), Relationship::Contains, "120000");
        assert_eq!(time.value_type(), Some(ValueType::Time));
        assert_eq!(
            time.dataset().try_get_str(tags::TIME).as_deref(),
            Some("120000")
        );

        let uid = ContentItem::uid_ref(concept, Relationship::Contains, "1.2.3");
        assert_eq!(uid.value_type(), Some(ValueType::UidRef));
        assert_eq!(
            uid.dataset().try_get_str(tags::UID).as_deref(),
            Some("1.2.3")
        );
    }

    #[test]
    fn a_valid_dataset_becomes_a_content_item() {
        let item = ContentItem::text(concept(), Relationship::Contains, "Mass");
        let rebuilt = content_item_from_dataset(item.dataset().clone()).unwrap();
        assert_eq!(rebuilt.value_type(), Some(ValueType::Text));
    }

    #[test]
    fn a_scoord_item_writes_graphic_type_and_pairs() {
        let value =
            SpatialCoordinates::new(GraphicType::Polyline, &[(1.0, 2.0), (3.0, 4.5)]).unwrap();
        let item = ContentItem::scoord(concept(), Relationship::Contains, value.clone());
        assert_eq!(item.value_type(), Some(ValueType::Scoord));
        assert_eq!(
            item.dataset().try_get_str(tags::GRAPHIC_TYPE).as_deref(),
            Some("POLYLINE")
        );
        // FL: two pairs = four 4-byte values, and the VR really is FL (an FD
        // write would round-trip just as happily through value_count)
        assert_eq!(
            item.dataset().value_count(tags::GRAPHIC_DATA).unwrap(),
            4,
            "GraphicData holds one FL per coordinate"
        );
        assert_eq!(
            item.dataset().get_element(tags::GRAPHIC_DATA).unwrap().vr(),
            Vr::Fl,
            "C.18.6.1 makes GraphicData FL"
        );
        assert_eq!(item.scoord_value(), Some(value.clone()));
        assert_eq!(
            item.value(),
            Some(ContentValue::Scoord(value)),
            "the typed reader agrees with the builder"
        );
    }

    #[test]
    fn a_scoord3d_item_carries_its_frame_of_reference() {
        let value = SpatialCoordinates3D::new(
            GraphicType3D::Ellipsoid,
            &[
                (0.0, 0.0, 0.0),
                (1.0, 0.0, 0.0),
                (0.0, 2.0, 0.0),
                (0.0, 0.0, 2.0),
                (0.0, 0.0, 0.0),
                (0.0, 0.0, 3.0),
            ],
            "1.2.840.10008.1.2.3",
        )
        .unwrap();
        let item = ContentItem::scoord3d(concept(), Relationship::Contains, value.clone());
        assert_eq!(item.value_type(), Some(ValueType::Scoord3D));
        assert_eq!(
            item.dataset()
                .try_get_str(tags::REFERENCED_FRAME_OF_REFERENCE_UID)
                .as_deref(),
            Some("1.2.840.10008.1.2.3")
        );
        assert_eq!(
            item.dataset().value_count(tags::GRAPHIC_DATA).unwrap(),
            18,
            "six triplets are eighteen FL values"
        );
        assert_eq!(item.scoord3d_value(), Some(value));
    }

    #[test]
    fn a_tcoord_item_writes_one_reference_list() {
        let samples = TemporalCoordinates::new(
            TemporalRangeType::Segment,
            TemporalRange::SamplePositions(vec![1, 5]),
        )
        .unwrap();
        let item = ContentItem::tcoord(concept(), Relationship::Contains, samples.clone());
        assert_eq!(item.value_type(), Some(ValueType::Tcoord));
        assert_eq!(
            item.dataset()
                .value_count(tags::REFERENCED_SAMPLE_POSITIONS)
                .unwrap(),
            2
        );
        assert_eq!(
            item.dataset()
                .get_element(tags::REFERENCED_SAMPLE_POSITIONS)
                .unwrap()
                .vr(),
            Vr::Ul,
            "C.18.7.1 makes ReferencedSamplePositions UL"
        );
        assert_eq!(item.tcoord_value(), Some(samples));

        let offsets = TemporalCoordinates::new(
            TemporalRangeType::Point,
            TemporalRange::TimeOffsets(vec![0.5]),
        )
        .unwrap();
        let item = ContentItem::tcoord(concept(), Relationship::Contains, offsets.clone());
        assert_eq!(
            item.dataset()
                .try_get_str(tags::REFERENCED_TIME_OFFSETS)
                .as_deref(),
            Some("0.5")
        );
        assert_eq!(item.tcoord_value(), Some(offsets));

        let times = TemporalCoordinates::new(
            TemporalRangeType::Begin,
            TemporalRange::DateTimes(vec!["20260101120000".into()]),
        )
        .unwrap();
        let item = ContentItem::tcoord(concept(), Relationship::Contains, times.clone());
        assert_eq!(
            item.dataset()
                .try_get_str(tags::REFERENCED_DATE_TIME)
                .as_deref(),
            Some("20260101120000")
        );
        assert_eq!(item.tcoord_value(), Some(times));
    }

    #[test]
    fn a_waveform_item_carries_references_and_channels() {
        let reference = ReferencedSop::new("1.2.840.10008.5.1.4.1.1.9.1.1", "1.2.3");
        let item = ContentItem::waveform(
            concept(),
            Relationship::Contains,
            std::slice::from_ref(&reference),
            &[(1, 0), (3, 2)],
        );
        assert_eq!(item.value_type(), Some(ValueType::Waveform));
        assert_eq!(item.referenced_sops(), vec![reference.clone()]);
        assert_eq!(item.waveform_channels(), vec![(1, 0), (3, 2)]);
        assert_eq!(
            item.dataset()
                .try_get_sequence(tags::REFERENCED_SOP_SEQUENCE)
                .unwrap()
                .items()[0]
                .get_element(tags::REFERENCED_WAVEFORM_CHANNELS)
                .unwrap()
                .vr(),
            Vr::Us,
            "C.18.5.1 makes ReferencedWaveformChannels US"
        );
        assert_eq!(
            item.value(),
            Some(ContentValue::Waveform {
                references: vec![reference],
                channels: vec![(1, 0), (3, 2)],
            })
        );
    }

    #[test]
    fn external_waveform_references_keep_distinct_channel_selections() {
        let mut dataset = Dataset::new();
        dataset.insert_str(tags::VALUE_TYPE, Vr::Cs, "WAVEFORM");
        for (instance, group, channel) in [("1.2.3", 1u16, 2u16), ("1.2.4", 2, 3)] {
            let mut reference = Dataset::new();
            reference.insert_str(
                tags::REFERENCED_SOP_CLASS_UID,
                Vr::Ui,
                "1.2.840.10008.5.1.4.1.1.9.1.1",
            );
            reference.insert_str(tags::REFERENCED_SOP_INSTANCE_UID, Vr::Ui, instance);
            insert_bytes(
                &mut reference,
                tags::REFERENCED_WAVEFORM_CHANNELS,
                Vr::Us,
                [group.to_le_bytes(), channel.to_le_bytes()].concat(),
            );
            match dataset.sequence_mut(tags::REFERENCED_SOP_SEQUENCE) {
                Some(sequence) => sequence.push(reference),
                None => dataset
                    .insert_sequence(tags::REFERENCED_SOP_SEQUENCE)
                    .push(reference),
            }
        }
        let item = ContentItem::from_dataset(dataset);
        let references = item.waveform_references().unwrap();
        assert_eq!(references[0].reference.instance_uid, "1.2.3");
        assert_eq!(references[0].channels, [(1, 2)]);
        assert_eq!(references[1].reference.instance_uid, "1.2.4");
        assert_eq!(references[1].channels, [(2, 3)]);
        assert!(
            item.value().is_none(),
            "legacy common-channel view must not merge different selections"
        );
    }

    #[test]
    fn waveform_channels_are_nested_in_the_referenced_sop_item() {
        let item = ContentItem::waveform(
            concept(),
            Relationship::Contains,
            &[ReferencedSop::new("1.2.3", "1.2.4")],
            &[(1, 2)],
        );
        assert!(!item.dataset().contains(&tags::REFERENCED_WAVEFORM_CHANNELS));
        let reference = &item
            .dataset()
            .try_get_sequence(tags::REFERENCED_SOP_SEQUENCE)
            .unwrap()
            .items()[0];
        assert_eq!(
            reference
                .get_at::<u16>(&tags::REFERENCED_WAVEFORM_CHANNELS, 1)
                .unwrap(),
            2
        );
    }

    #[test]
    fn graphic_values_reject_the_wrong_point_count() {
        // POINT is exactly one pair, ELLIPSE exactly four (C.18.6.1.2)
        assert!(SpatialCoordinates::new(GraphicType::Point, &[]).is_err());
        assert!(SpatialCoordinates::new(GraphicType::Point, &[(1.0, 1.0), (2.0, 2.0)]).is_err());
        assert!(SpatialCoordinates::new(GraphicType::Ellipse, &[(1.0, 1.0); 3]).is_err());
        assert!(SpatialCoordinates::new(GraphicType::Ellipse, &[(1.0, 1.0); 4]).is_ok());
        // POLYLINE needs at least two
        assert!(SpatialCoordinates::new(GraphicType::Polyline, &[(1.0, 1.0)]).is_err());
        // ELLIPSOID is exactly six triplets, POLYGON at least three
        assert!(SpatialCoordinates3D::new(
            GraphicType3D::Ellipsoid,
            &[(0.0, 0.0, 0.0); 5],
            "1.2.3"
        )
        .is_err());
        assert!(
            SpatialCoordinates3D::new(GraphicType3D::Polygon, &[(0.0, 0.0, 0.0); 2], "1.2.3")
                .is_err()
        );
    }

    #[test]
    fn a_polygon_must_close_and_a_scoord3d_needs_a_frame_of_reference() {
        let open = [
            (0.0, 0.0, 0.0),
            (1.0, 0.0, 0.0),
            (1.0, 1.0, 0.0),
            (0.0, 1.0, 0.0),
        ];
        let error = SpatialCoordinates3D::new(GraphicType3D::Polygon, &open, "1.2.3")
            .unwrap_err()
            .to_string();
        assert!(error.contains("first and last"), "got {error}");

        let mut closed = open.to_vec();
        closed.push(open[0]);
        assert!(SpatialCoordinates3D::new(GraphicType3D::Polygon, &closed, "1.2.3").is_ok());
        assert!(SpatialCoordinates3D::new(GraphicType3D::Polygon, &closed, " ").is_err());
    }

    #[test]
    fn temporal_coordinates_check_their_range_type() {
        // SEGMENT is exactly two points, MULTISEGMENT an even number
        assert!(TemporalCoordinates::new(
            TemporalRangeType::Segment,
            TemporalRange::SamplePositions(vec![1])
        )
        .is_err());
        assert!(TemporalCoordinates::new(
            TemporalRangeType::Multisegment,
            TemporalRange::SamplePositions(vec![1, 2, 3])
        )
        .is_err());
        assert!(TemporalCoordinates::new(
            TemporalRangeType::Multisegment,
            TemporalRange::SamplePositions(vec![1, 2, 3, 4])
        )
        .is_ok());
        // a DT value must parse
        assert!(TemporalCoordinates::new(
            TemporalRangeType::Point,
            TemporalRange::DateTimes(vec!["not a date".into()])
        )
        .is_err());
    }

    #[test]
    fn every_value_type_builds_and_reads_back() {
        let name = concept();
        let units = CodeItem::new("mm", "UCUM");
        let reference = ReferencedSop::new("1.2.840.10008.5.1.4.1.1.2", "1.2.3");
        let scoord =
            SpatialCoordinates::new(GraphicType::Circle, &[(1.0, 1.0), (2.0, 2.0)]).unwrap();
        let scoord3d = SpatialCoordinates3D::new(
            GraphicType3D::Polyline,
            &[(0.0, 0.0, 0.0), (1.0, 1.0, 1.0)],
            "1.2.3",
        )
        .unwrap();
        let tcoord = TemporalCoordinates::new(
            TemporalRangeType::Point,
            TemporalRange::SamplePositions(vec![7]),
        )
        .unwrap();

        let items = vec![
            (
                ContentItem::container(
                    name.clone(),
                    Some(Relationship::Contains),
                    Some("SEPARATE"),
                ),
                ContentValue::Container,
            ),
            (
                ContentItem::text(name.clone(), Relationship::Contains, "t"),
                ContentValue::Text("t".into()),
            ),
            (
                ContentItem::code(name.clone(), Relationship::Contains, units.clone()),
                ContentValue::Code(units.clone()),
            ),
            (
                ContentItem::num(
                    name.clone(),
                    Relationship::Contains,
                    MeasuredValue::new(12.5, units),
                )
                .unwrap(),
                ContentValue::Num(MeasuredValue::new(12.5, CodeItem::new("mm", "UCUM"))),
            ),
            (
                ContentItem::person_name(name.clone(), Relationship::Contains, "Doe^J"),
                ContentValue::PersonName("Doe^J".into()),
            ),
            (
                ContentItem::date(name.clone(), Relationship::Contains, "20260101"),
                ContentValue::Date("20260101".into()),
            ),
            (
                ContentItem::time(name.clone(), Relationship::Contains, "120000"),
                ContentValue::Time("120000".into()),
            ),
            (
                ContentItem::date_time(name.clone(), Relationship::Contains, "20260101120000"),
                ContentValue::DateTime("20260101120000".into()),
            ),
            (
                ContentItem::uid_ref(name.clone(), Relationship::Contains, "1.2.3"),
                ContentValue::UidRef("1.2.3".into()),
            ),
            (
                ContentItem::referenced_sop(
                    name.clone(),
                    Relationship::Contains,
                    ValueType::Composite,
                    std::slice::from_ref(&reference),
                ),
                ContentValue::References(vec![reference.clone()]),
            ),
            (
                ContentItem::referenced_sop(
                    name.clone(),
                    Relationship::Contains,
                    ValueType::Image,
                    std::slice::from_ref(&reference),
                ),
                ContentValue::References(vec![reference.clone()]),
            ),
            (
                ContentItem::waveform(
                    name.clone(),
                    Relationship::Contains,
                    std::slice::from_ref(&reference),
                    &[(1, 1)],
                ),
                ContentValue::Waveform {
                    references: vec![reference],
                    channels: vec![(1, 1)],
                },
            ),
            (
                ContentItem::scoord(name.clone(), Relationship::Contains, scoord.clone()),
                ContentValue::Scoord(scoord),
            ),
            (
                ContentItem::scoord3d(name.clone(), Relationship::Contains, scoord3d.clone()),
                ContentValue::Scoord3D(scoord3d),
            ),
            (
                ContentItem::tcoord(name, Relationship::Contains, tcoord.clone()),
                ContentValue::Tcoord(tcoord),
            ),
        ];

        // all 15 value types, each one readable through `value()`
        assert_eq!(items.len(), 15);
        for (item, expected) in &items {
            assert_eq!(
                item.value().as_ref(),
                Some(expected),
                "{:?}",
                item.value_type()
            );
        }
    }
}
