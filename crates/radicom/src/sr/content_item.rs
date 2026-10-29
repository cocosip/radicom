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
    /// (0040,A730) also the sequence a container's children live in.
    pub const CONTINUITY_OF_CONTENT: Tag = Tag::CONTINUITY_OF_CONTENT;
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

/// One content item: the tagged dataset plus its typed value (§12.3).
#[derive(Debug, Clone)]
pub struct ContentItem {
    dataset: Dataset,
}

impl ContentItem {
    /// Starts a container with a concept name (§12.3).
    pub fn container(
        concept_name: CodeItem,
        relationship: Option<Relationship>,
        continuity: Option<&str>,
    ) -> Self {
        let mut item = Self::base(concept_name, ValueType::Container, relationship);
        if let Some(continuity) = continuity {
            item.dataset
                .insert_str(tags::CONTINUITY_OF_CONTENT, Vr::Cs, continuity);
        }
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
}
