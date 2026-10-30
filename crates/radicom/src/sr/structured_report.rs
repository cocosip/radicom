//! Structured Report document (SR-01/02; docs/01-architecture.md §12).
//!
//! A `StructuredReport` wraps a `DicomFile` whose dataset holds the SR content
//! tree. `open` reads a file, `save` writes it with explicit-length sequences,
//! matching fo-dicom's behaviour (§12.3).

use crate::error::Error;
use crate::io::file::DicomFile;
use crate::io::writer::WriteOptions;
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::model::vr::Vr;
use crate::sr::code_item::CodeItem;
use crate::sr::content_item::{tags, ContentItem, ValueType};
use std::path::Path;

/// Basic Text SR Storage, the SOP class a `StructuredReport` document is.
pub const BASIC_TEXT_SR_STORAGE: &str = crate::uids::BASIC_TEXT_SR_STORAGE;

/// A structured report document (§12.3).
#[derive(Debug, Clone)]
pub struct StructuredReport {
    file: DicomFile,
}

impl StructuredReport {
    /// Wraps an already-built `DicomFile`.
    pub fn from_file(file: DicomFile) -> Self {
        StructuredReport { file }
    }

    /// Wraps a dataset in a minimal SR file (§12.3).
    ///
    /// The document's own identity is filled in when the dataset does not carry
    /// it: a Basic Text SR instance must name its SOP class, the file meta
    /// group is built from it (review #8), and the document ValueType is
    /// CONTAINER (Type 1 in the SR Document Content Module).
    pub fn from_dataset(mut dataset: Dataset) -> Self {
        if !dataset.contains(&Tag::SOP_CLASS_UID) {
            dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, BASIC_TEXT_SR_STORAGE);
        }
        if !dataset.contains(&Tag::SOP_INSTANCE_UID) {
            dataset.insert_str(
                Tag::SOP_INSTANCE_UID,
                Vr::Ui,
                crate::model::uid::Uid::generate_value(),
            );
        }
        if !dataset.contains(&Tag::VALUE_TYPE) {
            dataset.insert_str(Tag::VALUE_TYPE, Vr::Cs, "CONTAINER");
        }
        StructuredReport {
            file: DicomFile::from_dataset(dataset),
        }
    }

    /// Opens an SR from a file (§12.3).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(StructuredReport {
            file: DicomFile::open(path)?,
        })
    }

    /// Opens an SR from any seekable reader (fo-dicom's `Stream` overload).
    pub fn open_reader<R: std::io::Read + std::io::Seek>(reader: &mut R) -> Result<Self, Error> {
        Ok(StructuredReport {
            file: DicomFile::from_reader(reader, &crate::io::file::ReadOptions::default())?,
        })
    }

    /// Reads an SR from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        Ok(StructuredReport {
            file: DicomFile::from_bytes(bytes, &crate::io::file::ReadOptions::default())?,
        })
    }

    /// The underlying file.
    pub fn file(&self) -> &DicomFile {
        &self.file
    }

    /// The document dataset.
    pub fn dataset(&self) -> &Dataset {
        &self.file.dataset
    }

    /// The content items of the root container: the top-level (0040,A730)
    /// ContentSequence (§12.3).
    ///
    /// The document dataset itself *is* the root content item (PS3.3 C.17.2:
    /// ValueType, ConceptNameCodeSequence and ContinuityOfContent live at the
    /// top level), so every item here is a child and carries a
    /// RelationshipType.
    pub fn content_items(&self) -> Vec<ContentItem> {
        self.file
            .dataset
            .try_get_sequence(tags::CONTENT_SEQUENCE)
            .map(|sequence| {
                sequence
                    .items()
                    .iter()
                    .map(|item| ContentItem::from_dataset(item.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Replaces the content items of the root container.
    pub fn set_content_items(&mut self, items: Vec<ContentItem>) -> &mut Self {
        let sequence = self.file.dataset.insert_sequence(tags::CONTENT_SEQUENCE);
        sequence.items_mut().clear();
        for item in items {
            sequence.push(item.into_dataset());
        }
        self
    }

    /// Writes `root` as the document's root container: its ValueType, concept
    /// name and ContinuityOfContent become top-level attributes and its children
    /// become the ContentSequence (PS3.3 C.17.2).
    pub fn set_root(&mut self, root: ContentItem) -> &mut Self {
        let root = root.into_dataset();
        for tag in [
            Tag::VALUE_TYPE,
            tags::CONCEPT_NAME,
            tags::CONTINUITY_OF_CONTENT,
        ] {
            if let Some(item) = root.item(&tag) {
                self.file.dataset.insert_or_update(item.clone());
            }
        }
        let children: Vec<ContentItem> = root
            .try_get_sequence(tags::CONTENT_SEQUENCE)
            .map(|sequence| {
                sequence
                    .items()
                    .iter()
                    .map(|item| ContentItem::from_dataset(item.clone()))
                    .collect()
            })
            .unwrap_or_default();
        self.set_content_items(children)
    }

    /// The document's own value type, which is CONTAINER for an SR (§12.3).
    pub fn root_value_type(&self) -> Option<ValueType> {
        self.file
            .dataset
            .try_get_str(Tag::VALUE_TYPE)
            .and_then(|text| ValueType::parse(&text))
    }

    /// The root container's concept name (§12.3).
    pub fn root_concept_name(&self) -> Option<CodeItem> {
        let sequence = self.file.dataset.try_get_sequence(tags::CONCEPT_NAME)?;
        sequence
            .items()
            .first()
            .and_then(|item| CodeItem::read_from(item).ok())
    }

    /// Validates the document against the SR IOD (§12.3; PS3.3 C.17.2, C.18.1).
    ///
    /// The document dataset is the root content item, so it must carry the Type 1
    /// attributes of the SR Document Content module — ValueType CONTAINER, a
    /// concept name and ContinuityOfContent — plus the SR Document General
    /// module's SOP identity and CompletionFlag. Every top-level ContentSequence
    /// item is then validated as a child: a known ValueType, a concept name, a
    /// RelationshipType, the value its ValueType requires, and the same rules for
    /// its own children.
    pub fn validate(&self) -> Result<(), Error> {
        let dataset = &self.file.dataset;
        for tag in [Tag::SOP_CLASS_UID, Tag::SOP_INSTANCE_UID] {
            if !dataset.contains(&tag) {
                return Err(invalid(format!("the SR lacks {tag}")));
            }
        }
        match dataset.try_get_str(Tag::VALUE_TYPE).as_deref() {
            Some("CONTAINER") => {}
            Some(other) => {
                return Err(invalid(format!(
                    "the SR document ValueType is `{other}`, not CONTAINER"
                )))
            }
            None => return Err(invalid("the SR document lacks a ValueType".into())),
        }
        if self.root_concept_name().is_none() {
            return Err(invalid(
                "the SR document lacks a ConceptNameCodeSequence".into(),
            ));
        }
        match dataset.try_get_str(tags::CONTINUITY_OF_CONTENT).as_deref() {
            Some("SEPARATE") | Some("CONTINUOUS") => {}
            Some(other) => {
                return Err(invalid(format!(
                    "ContinuityOfContent `{other}` is neither SEPARATE nor CONTINUOUS"
                )))
            }
            None => {
                return Err(invalid(
                    "the SR document lacks a ContinuityOfContent".into(),
                ))
            }
        }
        match dataset.try_get_str(tags::COMPLETION_FLAG).as_deref() {
            Some("PARTIAL") | Some("COMPLETE") => {}
            Some(other) => {
                return Err(invalid(format!(
                    "CompletionFlag `{other}` is neither PARTIAL nor COMPLETE"
                )))
            }
            None => return Err(invalid("the SR lacks a CompletionFlag".into())),
        }
        if let Some(flag) = dataset.try_get_str(tags::VERIFICATION_FLAG) {
            if flag != "VERIFIED" && flag != "UNVERIFIED" {
                return Err(invalid(format!(
                    "VerificationFlag `{flag}` is neither VERIFIED nor UNVERIFIED"
                )));
            }
        }

        for item in self.content_items() {
            validate_item(&item)?;
        }
        Ok(())
    }

    /// Sets `(0040,A491)` CompletionFlag: `PARTIAL` or `COMPLETE` (§12.3).
    pub fn set_completion_flag(&mut self, value: impl Into<String>) -> &mut Self {
        self.file
            .dataset
            .insert_str(tags::COMPLETION_FLAG, Vr::Cs, value);
        self
    }

    /// The CompletionFlag, when present.
    pub fn completion_flag(&self) -> Option<String> {
        self.file.dataset.try_get_str(tags::COMPLETION_FLAG)
    }

    /// Sets `(0040,A493)` VerificationFlag: `VERIFIED` or `UNVERIFIED` (§12.3).
    pub fn set_verification_flag(&mut self, value: impl Into<String>) -> &mut Self {
        self.file
            .dataset
            .insert_str(tags::VERIFICATION_FLAG, Vr::Cs, value);
        self
    }

    /// The VerificationFlag, when present.
    pub fn verification_flag(&self) -> Option<String> {
        self.file.dataset.try_get_str(tags::VERIFICATION_FLAG)
    }

    /// The write options matching fo-dicom's SR behaviour (§12.3).
    fn write_options() -> WriteOptions {
        WriteOptions {
            explicit_length_sequences: true,
            explicit_length_sequence_items: true,
            keep_group_lengths: false,
            ..Default::default()
        }
    }

    /// Saves with explicit-length sequences/items (§12.3, fo-dicom behaviour).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        let bytes = self.file.to_bytes(&Self::write_options())?;
        std::fs::write(path, bytes)
            .map_err(|e| Error::File(crate::error::FileError::Other(format!("write: {e}"))))
    }

    /// Writes the SR to any writer (fo-dicom's `Stream` overload), using the
    /// same explicit-length options as [`Self::save`].
    pub fn write_to<W: std::io::Write>(&self, writer: &mut W) -> Result<(), Error> {
        self.file.write(writer, &Self::write_options())
    }
}

/// A validation failure of the SR document or one of its content items.
fn invalid(message: String) -> Error {
    Error::Data(crate::error::DataError::Other { message })
}

/// ConceptNameCodeSequence is Type 1C (Table C.17-5): required for these value
/// types; a CONTAINER without a heading, and the reference value types whose
/// concept name only conveys a purpose of reference, may omit it.
fn concept_name_required(value_type: ValueType) -> bool {
    matches!(
        value_type,
        ValueType::Text
            | ValueType::Num
            | ValueType::Code
            | ValueType::DateTime
            | ValueType::Date
            | ValueType::Time
            | ValueType::UidRef
            | ValueType::PersonName
    )
}

/// Validates one content item and its children (§12.3; PS3.3 C.17.3, C.18.1).
///
/// Every content item is a child of the document root (or of another item), so
/// each one needs a RelationshipType, the concept name the standard makes
/// conditional and the value its ValueType requires. Any item may carry children
/// (Table C.17-6), and a CONTAINER must state its ContinuityOfContent.
fn validate_item(item: &ContentItem) -> Result<(), Error> {
    // Table C.17-6: a by-reference target carries only a RelationshipType and the
    // identifier of the target item - no ValueType, concept name or value.
    if item
        .dataset()
        .contains(&tags::REFERENCED_CONTENT_ITEM_IDENTIFIER)
    {
        if item.relationship().is_none() {
            return Err(invalid(
                "a by-reference content item lacks a RelationshipType".into(),
            ));
        }
        return Ok(());
    }
    let value_type = item
        .value_type()
        .ok_or_else(|| invalid("a content item lacks a known ValueType".into()))?;
    let name = value_type.as_str();
    if concept_name_required(value_type) && item.concept_name().is_none() {
        return Err(invalid(format!(
            "a {name} item lacks a ConceptNameCodeSequence"
        )));
    }
    if item.relationship().is_none() {
        return Err(invalid(format!("a {name} item lacks a RelationshipType")));
    }
    if !item.has_value(value_type) {
        return Err(invalid(format!("a {name} item lacks its value tag")));
    }
    let children = item.children();
    if value_type == ValueType::Container {
        match item
            .dataset()
            .try_get_str(tags::CONTINUITY_OF_CONTENT)
            .as_deref()
        {
            Some("SEPARATE") | Some("CONTINUOUS") => {}
            Some(other) => {
                return Err(invalid(format!(
                    "a CONTAINER item has ContinuityOfContent `{other}`"
                )))
            }
            None => {
                return Err(invalid(
                    "a CONTAINER item lacks a ContinuityOfContent".into(),
                ))
            }
        }
    }
    // Table C.17-6: a ContentSequence holds one or more items
    if item.dataset().contains(&tags::CONTENT_SEQUENCE) && children.is_empty() {
        return Err(invalid(format!(
            "a {name} item has an empty ContentSequence"
        )));
    }
    for child in &children {
        validate_item(child)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sr::code_item::CodeItem;
    use crate::sr::content_item::{
        ContentValue, GraphicType, GraphicType3D, ReferencedSop, Relationship, SpatialCoordinates,
        SpatialCoordinates3D, TemporalCoordinates, TemporalRange, TemporalRangeType,
    };

    fn container() -> ContentItem {
        ContentItem::container(
            CodeItem::new("126000", "DCM").with_meaning("Imaging Measurement Report"),
            None,
            Some("SEPARATE"),
        )
    }

    /// A conformant document: SOP identity and document ValueType from
    /// `from_dataset`, plus the Type 1 flags.
    fn report_with(root: ContentItem) -> StructuredReport {
        let mut report = StructuredReport::from_dataset(Dataset::new());
        report.set_completion_flag("PARTIAL");
        report.set_verification_flag("UNVERIFIED");
        report.set_root(root);
        report
    }

    #[test]
    fn a_report_round_trips_through_bytes() {
        let mut root = container();
        root.add_child(ContentItem::text(
            CodeItem::new("121071", "DCM"),
            Relationship::Contains,
            "Mass in the left lung",
        ));

        let report = report_with(root);
        report.validate().unwrap();
        assert_eq!(report.root_value_type(), Some(ValueType::Container));
        assert_eq!(report.completion_flag().as_deref(), Some("PARTIAL"));

        let bytes = {
            let options = WriteOptions {
                explicit_length_sequences: true,
                explicit_length_sequence_items: true,
                keep_group_lengths: false,
                ..Default::default()
            };
            report.file().to_bytes(&options).unwrap()
        };
        let reopened = StructuredReport::from_bytes(&bytes).unwrap();
        reopened.validate().unwrap();
        let items = reopened.content_items();
        assert_eq!(items.len(), 1, "the root container has one child");
        assert_eq!(
            items[0].text_value().as_deref(),
            Some("Mass in the left lung")
        );
        assert_eq!(
            reopened.root_concept_name().map(|code| code.value),
            Some("126000".to_string()),
            "the root concept name lives at the top level"
        );
    }

    #[test]
    fn a_document_without_a_root_heading_fails_validation() {
        let report = StructuredReport::from_dataset(Dataset::new());
        let error = report.validate().unwrap_err().to_string();
        assert!(
            error.contains("ConceptNameCodeSequence"),
            "the root needs its document title, got {error}"
        );
        assert_eq!(report.root_value_type(), Some(ValueType::Container));
        assert_eq!(report.root_concept_name(), None);
    }

    #[test]
    fn a_text_root_fails_validation() {
        let report = report_with(ContentItem::text(
            CodeItem::new("121071", "DCM"),
            Relationship::Contains,
            "not a container",
        ));
        let error = report.validate().unwrap_err().to_string();
        assert!(
            error.contains("not CONTAINER"),
            "a TEXT document is not an SR, got {error}"
        );
    }

    #[test]
    fn a_report_without_a_completion_flag_fails_validation() {
        let mut root = container();
        root.add_child(ContentItem::text(
            CodeItem::new("121071", "DCM"),
            Relationship::Contains,
            "ok",
        ));
        let mut report = StructuredReport::from_dataset(Dataset::new());
        report.set_root(root);
        let error = report.validate().unwrap_err().to_string();
        assert!(error.contains("CompletionFlag"), "got {error}");

        // and a value outside PARTIAL/COMPLETE is rejected too
        report.set_completion_flag("DONE");
        let error = report.validate().unwrap_err().to_string();
        assert!(error.contains("DONE"), "got {error}");
    }

    #[test]
    fn a_child_without_a_relationship_type_fails_validation() {
        let mut root = container();
        // a child built without a relationship: a CONTAINER with `None`
        root.add_child(ContentItem::container(
            CodeItem::new("121071", "DCM"),
            None,
            None,
        ));
        let report = report_with(root);
        let error = report.validate().unwrap_err().to_string();
        assert!(error.contains("RelationshipType"), "got {error}");
    }

    #[test]
    fn an_item_without_its_value_tag_fails_validation() {
        let mut root = container();
        // a TEXT item whose TextValue was dropped
        let mut child = ContentItem::text(
            CodeItem::new("121071", "DCM"),
            Relationship::Contains,
            "lost",
        );
        let mut dataset = child.into_dataset();
        dataset.remove(tags::TEXT_VALUE);
        child = ContentItem::from_dataset(dataset);
        root.add_child(child);
        let report = report_with(root);
        let error = report.validate().unwrap_err().to_string();
        assert!(
            error.contains("TEXT") && error.contains("value"),
            "got {error}"
        );
    }

    #[test]
    fn graphic_and_temporal_values_survive_a_write_and_read() {
        let mut root = container();
        let name = CodeItem::new("121071", "DCM");
        let scoord = SpatialCoordinates::new(
            GraphicType::Ellipse,
            &[(1.0, 2.0), (3.0, 4.0), (5.0, 6.0), (7.0, 8.0)],
        )
        .unwrap();
        root.add_child(ContentItem::scoord(
            name.clone(),
            Relationship::Contains,
            scoord.clone(),
        ));
        let scoord3d = SpatialCoordinates3D::new(
            GraphicType3D::Polyline,
            &[(1.0, 2.0, 3.0), (4.0, 5.0, 6.0)],
            "1.2.840.10008.1.2.3",
        )
        .unwrap();
        root.add_child(ContentItem::scoord3d(
            name.clone(),
            Relationship::Contains,
            scoord3d.clone(),
        ));
        let tcoord = TemporalCoordinates::new(
            TemporalRangeType::Segment,
            TemporalRange::SamplePositions(vec![1, 5]),
        )
        .unwrap();
        root.add_child(ContentItem::tcoord(
            name.clone(),
            Relationship::Contains,
            tcoord.clone(),
        ));
        let reference = ReferencedSop::new("1.2.840.10008.5.1.4.1.1.9.1.1", "1.2.3");
        root.add_child(ContentItem::waveform(
            name,
            Relationship::Contains,
            std::slice::from_ref(&reference),
            &[(1, 2)],
        ));

        let report = report_with(root);
        let mut bytes = Vec::new();
        report.write_to(&mut bytes).unwrap();
        let reopened = StructuredReport::from_bytes(&bytes).unwrap();
        reopened.validate().unwrap();

        let children = reopened.content_items();
        assert_eq!(children.len(), 4);
        assert_eq!(children[0].scoord_value(), Some(scoord));
        assert_eq!(children[1].scoord3d_value(), Some(scoord3d));
        assert_eq!(children[2].tcoord_value(), Some(tcoord));
        assert_eq!(
            children[3].value(),
            Some(ContentValue::Waveform {
                references: vec![reference],
                channels: vec![(1, 2)],
            })
        );
    }

    #[test]
    fn save_writes_a_file_that_opens_back() {
        let mut root = container();
        root.add_child(ContentItem::text(
            CodeItem::new("121071", "DCM"),
            Relationship::Contains,
            "ok",
        ));
        let report = report_with(root);

        let path = std::env::temp_dir().join(format!("radicom-sr-{}.dcm", std::process::id()));
        report.save(&path).unwrap();
        let reopened = StructuredReport::open(&path).unwrap();
        reopened.validate().unwrap();
        assert_eq!(reopened.content_items().len(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_report_round_trips_through_a_writer_and_reader() {
        let mut root = container();
        root.add_child(ContentItem::text(
            CodeItem::new("121071", "DCM"),
            Relationship::Contains,
            "stream",
        ));
        let report = report_with(root);

        let mut bytes = Vec::new();
        report.write_to(&mut bytes).unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        let reopened = StructuredReport::open_reader(&mut cursor).unwrap();
        reopened.validate().unwrap();
        assert_eq!(
            reopened.content_items()[0].text_value().as_deref(),
            Some("stream")
        );
    }

    /// The reference SR is written with explicit VR little endian, so a big
    /// endian round trip is the one case that exercises the writer's byte swap
    /// for GraphicData (FL) and ReferencedSamplePositions (UL).
    #[test]
    fn graphic_and_temporal_values_survive_a_big_endian_round_trip() {
        let mut root = container();
        let name = CodeItem::new("121071", "DCM");
        let scoord =
            SpatialCoordinates::new(GraphicType::Polyline, &[(1.0, 2.0), (3.0, 4.5)]).unwrap();
        root.add_child(ContentItem::scoord(
            name.clone(),
            Relationship::Contains,
            scoord.clone(),
        ));
        let tcoord = TemporalCoordinates::new(
            TemporalRangeType::Segment,
            TemporalRange::SamplePositions(vec![1, 5]),
        )
        .unwrap();
        root.add_child(ContentItem::tcoord(
            name,
            Relationship::Contains,
            tcoord.clone(),
        ));

        let mut dataset = Dataset::new();
        dataset.set_transfer_syntax(
            crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone(),
        );
        let mut report = StructuredReport::from_dataset(dataset);
        report.set_completion_flag("COMPLETE");
        report.set_root(root);

        let mut bytes = Vec::new();
        report.write_to(&mut bytes).unwrap();
        let reopened = StructuredReport::from_bytes(&bytes).unwrap();
        assert_eq!(
            reopened.dataset().transfer_syntax().uid(),
            "1.2.840.10008.1.2.2",
            "the document kept its big endian syntax"
        );
        let items = reopened.content_items();
        assert_eq!(items[0].scoord_value(), Some(scoord));
        assert_eq!(items[1].tcoord_value(), Some(tcoord));
    }

    /// The DCMTK reference SR has the root container in the dataset itself and
    /// every ContentSequence item with a RelationshipType (PS3.3 C.17.2).
    #[test]
    fn a_real_sr_document_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests-data/sr/test_SR.dcm");
        let Ok(bytes) = std::fs::read(&path) else {
            return; // packaged crate without the corpus
        };
        let report = StructuredReport::from_bytes(&bytes).unwrap();
        report
            .validate()
            .expect("the reference SR is a conformant document");
        assert_eq!(report.root_value_type(), Some(ValueType::Container));
        assert_eq!(
            report.root_concept_name().map(|code| code.value),
            Some("1111".to_string()),
            "the root concept name is the top-level one"
        );
        let items = report.content_items();
        assert_eq!(items.len(), 5);
        assert!(
            items.iter().all(|item| item.relationship().is_some()),
            "every top-level item carries a RelationshipType"
        );
    }
}
