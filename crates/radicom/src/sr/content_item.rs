//! SR content items (SR-01; docs/08-serialization-media.md §4.1).
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

/// Tags used by a content item (§4.1).
pub mod tags {
    use crate::model::tag::Tag;

    /// (0040,A730) ContentSequence.
    pub const CONTENT_SEQUENCE: Tag = Tag::new(0x0040, 0xA730);
    /// (0040,A040) ValueType.
    pub const VALUE_TYPE: Tag = Tag::new(0x0040, 0xA040);
    /// (0040,A010) RelationshipType.
    pub const RELATIONSHIP_TYPE: Tag = Tag::new(0x0040, 0xA010);
    /// (0040,A043) ConceptNameCodeSequence.
    pub const CONCEPT_NAME: Tag = Tag::new(0x0040, 0xA043);
    /// (0040,A160) TextValue.
    pub const TEXT_VALUE: Tag = Tag::new(0x0040, 0xA160);
    /// (0040,A123) PersonName.
    pub const PERSON_NAME: Tag = Tag::new(0x0040, 0xA123);
    /// (0040,A120) DateTime.
    pub const DATE_TIME: Tag = Tag::new(0x0040, 0xA120);
    /// (0040,A121) Date.
    pub const DATE: Tag = Tag::new(0x0040, 0xA121);
    /// (0040,A122) Time.
    pub const TIME: Tag = Tag::new(0x0040, 0xA122);
    /// (0040,A124) UID.
    pub const UID: Tag = Tag::new(0x0040, 0xA124);
    /// (0040,A168) ConceptCodeSequence.
    pub const CONCEPT_CODE_SEQUENCE: Tag = Tag::new(0x0040, 0xA168);
    /// (0040,A300) MeasuredValueSequence.
    pub const MEASURED_VALUE_SEQUENCE: Tag = Tag::new(0x0040, 0xA300);
    /// (0040,A730) also the sequence a container's children live in.
    pub const CONTINUITY_OF_CONTENT: Tag = Tag::new(0x0040, 0xA050);
}

/// The 15 SR value types (§4.1, PS3.3 C.18.1).
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

/// The 7 relationship types (§4.1, PS3.3 C.18.1), plus root.
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

/// One content item: the tagged dataset plus its typed value (§4.1).
#[derive(Debug, Clone)]
pub struct ContentItem {
    dataset: Dataset,
}

impl ContentItem {
    /// Starts a container with a concept name (§4.1).
    pub fn container(concept_name: CodeItem, relationship: Option<Relationship>, continuity: Option<&str>) -> Self {
        let mut item = Self::base(concept_name, ValueType::Container, relationship);
        if let Some(continuity) = continuity {
            item.dataset.insert_str(tags::CONTINUITY_OF_CONTENT, Vr::Cs, continuity);
        }
        item
    }

    /// A TEXT value (§4.1).
    pub fn text(concept_name: CodeItem, relationship: Relationship, text: impl Into<String>) -> Self {
        let mut item = Self::base(concept_name, ValueType::Text, Some(relationship));
        item.dataset.insert_str(tags::TEXT_VALUE, Vr::Ut, text);
        item
    }

    /// A CODE value: the code is written as a ConceptCodeSequence item (§4.1).
    pub fn code(concept_name: CodeItem, relationship: Relationship, code: CodeItem) -> Self {
        let mut item = Self::base(concept_name, ValueType::Code, Some(relationship));
        let mut coded = Dataset::new();
        code.write_into(&mut coded);
        item.dataset.insert_sequence(tags::CONCEPT_CODE_SEQUENCE).push(coded);
        item
    }

    /// A PNAME value (§4.1).
    pub fn person_name(concept_name: CodeItem, relationship: Relationship, name: impl Into<String>) -> Self {
        let mut item = Self::base(concept_name, ValueType::PersonName, Some(relationship));
        item.dataset.insert_str(tags::PERSON_NAME, Vr::Pn, name);
        item
    }

    /// A DATETIME value (§4.1).
    pub fn date_time(concept_name: CodeItem, relationship: Relationship, value: impl Into<String>) -> Self {
        let mut item = Self::base(concept_name, ValueType::DateTime, Some(relationship));
        item.dataset.insert_str(tags::DATE_TIME, Vr::Dt, value);
        item
    }

    /// A DATE value (§4.1).
    pub fn date(concept_name: CodeItem, relationship: Relationship, value: impl Into<String>) -> Self {
        let mut item = Self::base(concept_name, ValueType::Date, Some(relationship));
        item.dataset.insert_str(tags::DATE, Vr::Da, value);
        item
    }

    /// A TIME value (§4.1).
    pub fn time(concept_name: CodeItem, relationship: Relationship, value: impl Into<String>) -> Self {
        let mut item = Self::base(concept_name, ValueType::Time, Some(relationship));
        item.dataset.insert_str(tags::TIME, Vr::Tm, value);
        item
    }

    /// A UIDREF value (§4.1).
    pub fn uid_ref(concept_name: CodeItem, relationship: Relationship, uid: impl Into<String>) -> Self {
        let mut item = Self::base(concept_name, ValueType::UidRef, Some(relationship));
        item.dataset.insert_str(tags::UID, Vr::Ui, uid);
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
        self.dataset.try_get_str(tags::VALUE_TYPE).and_then(|s| ValueType::parse(&s))
    }

    pub fn relationship(&self) -> Option<Relationship> {
        self.dataset.try_get_str(tags::RELATIONSHIP_TYPE).and_then(|s| Relationship::parse(&s))
    }

    /// The concept name code, when present.
    pub fn concept_name(&self) -> Option<CodeItem> {
        let sequence = self.dataset.try_get_sequence(tags::CONCEPT_NAME)?;
        sequence.items().first().and_then(|item| CodeItem::read_from(item).ok())
    }

    /// The text value, if this is a TEXT item.
    pub fn text_value(&self) -> Option<String> {
        self.dataset.try_get_str(tags::TEXT_VALUE)
    }

    /// The children of a container, read from ContentSequence (§4.1).
    pub fn children(&self) -> Vec<ContentItem> {
        self.dataset
            .try_get_sequence(tags::CONTENT_SEQUENCE)
            .map(|sequence| {
                sequence.items().iter().map(|item| ContentItem { dataset: item.clone() }).collect()
            })
            .unwrap_or_default()
    }

    /// Appends a child item to a container's ContentSequence (§4.1).
    ///
    /// `insert_sequence` replaces, so the existing items are moved into the
    /// new sequence first.
    pub fn add_child(&mut self, child: ContentItem) -> &mut Self {
        let mut items: Vec<Dataset> = self
            .dataset
            .try_get_sequence(tags::CONTENT_SEQUENCE)
            .map(|sequence| sequence.items().to_vec())
            .unwrap_or_default();
        items.push(child.dataset);
        *self.dataset.insert_sequence(tags::CONTENT_SEQUENCE).items_mut() = items;
        self
    }

    /// The shared header: concept name, value type and relationship (§4.1).
    fn base(concept_name: CodeItem, value_type: ValueType, relationship: Option<Relationship>) -> Self {
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
        None => Err(Error::Data(DataError::Other { message: "content item lacks a valid ValueType".into() })),
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
            assert_eq!(Relationship::parse(relationship.as_str()), Some(relationship));
        }
    }

    #[test]
    fn a_container_holds_children_in_its_content_sequence() {
        let mut root = ContentItem::container(concept(), None, Some("SEPARATE"));
        root.add_child(ContentItem::text(concept(), Relationship::Contains, "child 1"));
        root.add_child(ContentItem::text(concept(), Relationship::Contains, "child 2"));

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
        let item = ContentItem::code(concept(), Relationship::Contains, CodeItem::new("T-D0050", "SRT"));
        assert_eq!(item.value_type(), Some(ValueType::Code));
        let sequence = item.dataset().try_get_sequence(tags::CONCEPT_CODE_SEQUENCE).unwrap();
        assert_eq!(CodeItem::read_from(&sequence.items()[0]).unwrap().value, "T-D0050");
    }

    #[test]
    fn typed_scalar_items_write_their_value_tag() {
        let date = ContentItem::date(concept(), Relationship::HasObsContext, "20260101");
        assert_eq!(date.dataset().try_get_str(tags::DATE).as_deref(), Some("20260101"));
        let dt = ContentItem::date_time(concept(), Relationship::HasObsContext, "20260101120000");
        assert_eq!(dt.dataset().try_get_str(tags::DATE_TIME).as_deref(), Some("20260101120000"));
        let uid = ContentItem::uid_ref(concept(), Relationship::InferredFrom, "1.2.3");
        assert_eq!(uid.dataset().try_get_str(tags::UID).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn building_from_a_dataset_without_a_value_type_errors() {
        assert!(content_item_from_dataset(Dataset::new()).is_err());
    }
}
