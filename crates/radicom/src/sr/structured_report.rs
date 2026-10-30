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
use crate::sr::content_item::{
    tags, ContentItem, MeasuredValue, ReferencedSop, SpatialCoordinates, SpatialCoordinates3D,
    TemporalCoordinates, ValueType,
};
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
            self.file.dataset.remove(&tag);
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
            required_text(dataset, tag, Vr::Ui)?;
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
        let flag = dataset
            .try_get_str(tags::VERIFICATION_FLAG)
            .ok_or_else(|| invalid("the SR lacks a VerificationFlag".into()))?;
        if flag != "VERIFIED" && flag != "UNVERIFIED" {
            return Err(invalid(format!("invalid VerificationFlag `{flag}`")));
        }
        for (tag, vr) in [
            (Tag::CONTENT_DATE, Vr::Da),
            (Tag::CONTENT_TIME, Vr::Tm),
            (Tag::INSTANCE_NUMBER, Vr::Is),
        ] {
            required_text(dataset, tag, vr)?;
        }
        if flag == "VERIFIED" {
            if self.completion_flag().as_deref() != Some("COMPLETE") {
                return Err(invalid("VERIFIED requires COMPLETE".into()));
            }
            let observers = required_sequence(dataset, Tag::VERIFYING_OBSERVER_SEQUENCE, 1, None)?;
            for observer in observers.items() {
                for (tag, vr) in [
                    (Tag::VERIFYING_OBSERVER_NAME, Vr::Pn),
                    (Tag::VERIFYING_ORGANIZATION, Vr::Lo),
                    (Tag::VERIFICATION_DATE_TIME, Vr::Dt),
                ] {
                    required_text(observer, tag, vr)?;
                }
                let identification = required_sequence(
                    observer,
                    Tag::VERIFYING_OBSERVER_IDENTIFICATION_CODE_SEQUENCE,
                    0,
                    Some(1),
                )?;
                for code in identification.items() {
                    CodeItem::validate_dataset(code)?;
                }
            }
        }
        for tag in [
            tags::VALUE_TYPE,
            tags::COMPLETION_FLAG,
            tags::VERIFICATION_FLAG,
            tags::CONTINUITY_OF_CONTENT,
        ] {
            required_text(dataset, tag, Vr::Cs)?;
        }
        validate_code_sequence(dataset, tags::CONCEPT_NAME)?;
        for code in
            required_sequence(dataset, Tag::PERFORMED_PROCEDURE_CODE_SEQUENCE, 0, None)?.items()
        {
            CodeItem::validate_dataset(code)?;
        }
        if dataset.contains(&tags::CONTENT_SEQUENCE) {
            required_sequence(dataset, tags::CONTENT_SEQUENCE, 0, None)?;
        }
        for (tag, vr) in [
            (Tag::STUDY_INSTANCE_UID, Vr::Ui),
            (Tag::SERIES_INSTANCE_UID, Vr::Ui),
            (Tag::MODALITY, Vr::Cs),
            (Tag::SERIES_NUMBER, Vr::Is),
        ] {
            required_text(dataset, tag, vr)?;
        }
        if dataset.try_get_str(Tag::MODALITY).as_deref() != Some("SR") {
            return Err(invalid("SR Modality must be SR".into()));
        }
        for (tag, vr) in [
            (Tag::PATIENT_NAME, Vr::Pn),
            (Tag::PATIENT_ID, Vr::Lo),
            (Tag::PATIENT_BIRTH_DATE, Vr::Da),
            (Tag::PATIENT_SEX, Vr::Cs),
            (Tag::STUDY_DATE, Vr::Da),
            (Tag::STUDY_TIME, Vr::Tm),
            (Tag::REFERRING_PHYSICIAN_NAME, Vr::Pn),
            (Tag::STUDY_ID, Vr::Sh),
            (Tag::ACCESSION_NUMBER, Vr::Sh),
            (Tag::MANUFACTURER, Vr::Lo),
        ] {
            let element = dataset.get_element(&tag)?;
            if element.vr() != vr {
                return Err(invalid(format!("incorrect VR for Type 2 {tag}")));
            }
            let value = dataset.get_str(&tag)?;
            let decoded = crate::model::element::DicomElement::new_str(tag, vr, value);
            crate::validation::validate_element(&decoded)?;
        }
        if dataset
            .try_get_str(Tag::PATIENT_SEX)
            .is_some_and(|sex| !matches!(sex.as_str(), "" | "M" | "F" | "O"))
        {
            return Err(invalid("invalid PatientSex".into()));
        }
        required_sequence(
            dataset,
            Tag::REFERENCED_PERFORMED_PROCEDURE_STEP_SEQUENCE,
            0,
            None,
        )?;
        for (index, item) in self.content_items().iter().enumerate() {
            validate_item(item, dataset, &[1, index as u32 + 1])?;
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

fn required_text(dataset: &Dataset, tag: Tag, vr: Vr) -> Result<String, Error> {
    let value = dataset
        .try_get_str(&tag)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid(format!("missing or empty Type 1 {tag}")))?;
    if dataset.get_element(&tag)?.vr() != vr || (vr.is_multi_value() && value.contains('\\')) {
        return Err(invalid(format!("incorrect VR or multiplicity for {tag}")));
    }
    crate::validation::validate_single(vr, &value).map_err(invalid)?;
    Ok(value)
}

fn required_numeric(dataset: &Dataset, tag: Tag, vr: Vr) -> Result<(), Error> {
    let element = dataset.get_element(&tag)?;
    if element.vr() != vr {
        return Err(invalid(format!("incorrect numeric VR for {tag}")));
    }
    if let crate::model::element::ElementValue::Bytes(buffer) = element.value() {
        if !buffer.size().is_multiple_of(vr.unit_size() as u64) {
            return Err(invalid(format!("truncated numeric value for {tag}")));
        }
    }
    Ok(())
}

fn required_sequence(
    dataset: &Dataset,
    tag: Tag,
    min: usize,
    max: Option<usize>,
) -> Result<&crate::model::item::DicomSequence, Error> {
    let sequence = dataset
        .try_get_sequence(&tag)
        .ok_or_else(|| invalid(format!("missing sequence {tag}")))?;
    if sequence.items().len() < min || max.is_some_and(|max| sequence.items().len() > max) {
        return Err(invalid(format!("incorrect item count for {tag}")));
    }
    Ok(sequence)
}

fn validate_code_sequence(dataset: &Dataset, tag: Tag) -> Result<(), Error> {
    let sequence = required_sequence(dataset, tag, 1, Some(1))?;
    CodeItem::validate_dataset(&sequence.items()[0])
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
fn validate_item(item: &ContentItem, root: &Dataset, path: &[u32]) -> Result<(), Error> {
    required_text(item.dataset(), tags::RELATIONSHIP_TYPE, Vr::Cs)
        .map_err(|error| invalid(format!("invalid RelationshipType: {error}")))?;
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
        let dataset = item.dataset();
        for field in dataset.items() {
            if field.tag() != &tags::RELATIONSHIP_TYPE
                && field.tag() != &tags::REFERENCED_CONTENT_ITEM_IDENTIFIER
            {
                return Err(invalid(
                    "a by-reference item carries by-value attributes".into(),
                ));
            }
        }
        let tag = tags::REFERENCED_CONTENT_ITEM_IDENTIFIER;
        if dataset.get_element(&tag)?.vr() != Vr::Ul {
            return Err(invalid("reference identifier must be UL".into()));
        }
        let count = dataset.value_count(&tag)?;
        let identifiers = (0..count)
            .map(|index| dataset.get_at::<u32>(&tag, index))
            .collect::<Result<Vec<_>, _>>()?;
        if identifiers.first() != Some(&1)
            || identifiers.contains(&0)
            || path.starts_with(&identifiers)
        {
            return Err(invalid("invalid by-reference target path".into()));
        }
        let mut target = root;
        for index in &identifiers[1..] {
            target = target
                .try_get_sequence(tags::CONTENT_SEQUENCE)
                .and_then(|sequence| sequence.items().get(*index as usize - 1))
                .ok_or_else(|| invalid("by-reference target is outside the content tree".into()))?;
        }
        if target.contains(&tags::REFERENCED_CONTENT_ITEM_IDENTIFIER) {
            return Err(invalid(
                "a by-reference item cannot target another reference".into(),
            ));
        }
        return Ok(());
    }
    let value_type = item
        .value_type()
        .ok_or_else(|| invalid("a content item lacks a known ValueType".into()))?;
    required_text(item.dataset(), tags::VALUE_TYPE, Vr::Cs)?;
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
    let dataset = item.dataset();
    if dataset.contains(&tags::CONCEPT_NAME) {
        validate_code_sequence(dataset, tags::CONCEPT_NAME)?;
    }
    match value_type {
        ValueType::Text => {
            required_text(dataset, tags::TEXT_VALUE, Vr::Ut)?;
        }
        ValueType::PersonName => {
            required_text(dataset, tags::PERSON_NAME, Vr::Pn)?;
        }
        ValueType::Date => {
            required_text(dataset, tags::DATE, Vr::Da)?;
        }
        ValueType::Time => {
            required_text(dataset, tags::TIME, Vr::Tm)?;
        }
        ValueType::DateTime => {
            required_text(dataset, tags::DATE_TIME, Vr::Dt)?;
        }
        ValueType::UidRef => {
            required_text(dataset, tags::UID, Vr::Ui)?;
        }
        ValueType::Code => validate_code_sequence(dataset, tags::CONCEPT_CODE_SEQUENCE)?,
        ValueType::Num => {
            let sequence = required_sequence(dataset, tags::MEASURED_VALUE_SEQUENCE, 0, Some(1))?;
            for measured in sequence.items() {
                required_text(measured, tags::NUMERIC_VALUE, Vr::Ds)?;
                let value = MeasuredValue::read_from(measured)?;
                value.numeric_value_text()?;
                validate_code_sequence(measured, tags::MEASUREMENT_UNITS_CODE_SEQUENCE)?;
            }
        }
        ValueType::Composite | ValueType::Image | ValueType::Waveform => {
            let references = required_sequence(dataset, tags::REFERENCED_SOP_SEQUENCE, 1, Some(1))?;
            for reference in references.items() {
                required_text(reference, tags::REFERENCED_SOP_CLASS_UID, Vr::Ui)?;
                required_text(reference, tags::REFERENCED_SOP_INSTANCE_UID, Vr::Ui)?;
                ReferencedSop::read_from(reference)?;
            }
            if value_type == ValueType::Waveform
                && (dataset.contains(&tags::REFERENCED_WAVEFORM_CHANNELS)
                    || item.waveform_references().is_none())
            {
                return Err(invalid("malformed WAVEFORM channels".into()));
            }
        }
        ValueType::Tcoord => {
            required_text(dataset, tags::TEMPORAL_RANGE_TYPE, Vr::Cs)?;
            for (tag, vr) in [
                (tags::REFERENCED_SAMPLE_POSITIONS, Vr::Ul),
                (tags::REFERENCED_TIME_OFFSETS, Vr::Ds),
                (tags::REFERENCED_DATE_TIME, Vr::Dt),
            ] {
                if dataset.contains(&tag) {
                    required_numeric(dataset, tag, vr)?;
                }
            }
            let value = item
                .tcoord_value()
                .ok_or_else(|| invalid("malformed TCOORD".into()))?;
            TemporalCoordinates::new(value.range_type, value.range)?;
        }
        _ => {}
    }
    // C.18.6.1.2 / C.18.9.1.2: the GraphicType fixes how many coordinates the
    // value must have, so a wrong count is a conformance failure, not just a
    // renderer's guess.
    match value_type {
        ValueType::Scoord => {
            required_text(dataset, tags::GRAPHIC_TYPE, Vr::Cs)?;
            required_numeric(dataset, tags::GRAPHIC_DATA, Vr::Fl)?;
            let value = item
                .scoord_value()
                .ok_or_else(|| invalid("a SCOORD item has malformed GraphicData".into()))?;
            SpatialCoordinates::new(value.graphic_type, &value.points)?;
        }
        ValueType::Scoord3D => {
            required_text(dataset, tags::GRAPHIC_TYPE, Vr::Cs)?;
            required_text(dataset, tags::REFERENCED_FRAME_OF_REFERENCE_UID, Vr::Ui)?;
            required_numeric(dataset, tags::GRAPHIC_DATA, Vr::Fl)?;
            let value = item
                .scoord3d_value()
                .ok_or_else(|| invalid("a SCOORD3D item has malformed GraphicData".into()))?;
            SpatialCoordinates3D::new(
                value.graphic_type,
                &value.points,
                value.frame_of_reference_uid,
            )?;
        }
        _ => {}
    }
    let children = item.children();
    if value_type == ValueType::Container {
        required_text(dataset, tags::CONTINUITY_OF_CONTENT, Vr::Cs)?;
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
    for (index, child) in children.iter().enumerate() {
        let mut child_path = path.to_vec();
        child_path.push(index as u32 + 1);
        validate_item(child, root, &child_path)?;
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

    #[test]
    fn required_document_attributes_and_verified_conditions_are_checked() {
        for tag in [
            Tag::VERIFICATION_FLAG,
            Tag::CONTENT_DATE,
            Tag::CONTENT_TIME,
            Tag::INSTANCE_NUMBER,
        ] {
            let mut report = report_with(container());
            report.file.dataset.remove(&tag);
            assert!(report.validate().is_err(), "missing {tag}");
        }
        let mut report = report_with(container());
        report.set_verification_flag("VERIFIED");
        assert!(report.validate().is_err(), "PARTIAL cannot be VERIFIED");
        report.set_completion_flag("COMPLETE");
        assert!(report.validate().is_err(), "VERIFIED needs an observer");
    }

    #[test]
    fn imported_empty_text_and_malformed_temporal_values_are_rejected() {
        let mut root = container();
        root.add_child(ContentItem::text(
            CodeItem::new("1", "TEST").with_meaning("Finding"),
            Relationship::Contains,
            "",
        ));
        assert!(report_with(root).validate().is_err());
        let mut root = container();
        let mut child = ContentItem::tcoord(
            CodeItem::new("1", "TEST").with_meaning("Time"),
            Relationship::Contains,
            TemporalCoordinates::new(
                TemporalRangeType::Point,
                TemporalRange::SamplePositions(vec![1]),
            )
            .unwrap(),
        )
        .into_dataset();
        child.insert_str(tags::TEMPORAL_RANGE_TYPE, Vr::Cs, "SEGMENT");
        root.add_child(ContentItem::from_dataset(child));
        assert!(report_with(root).validate().is_err());
    }

    #[test]
    fn imported_values_obey_constructor_rules_and_type_two_num_is_valid() {
        let mut root = container();
        let mut child = Dataset::new();
        child.insert_str(tags::VALUE_TYPE, Vr::Cs, "NUM");
        child.insert_str(tags::RELATIONSHIP_TYPE, Vr::Cs, "CONTAINS");
        let mut name = Dataset::new();
        CodeItem::new("1", "TEST")
            .with_meaning("Measurement")
            .write_into(&mut name);
        child.insert_sequence(tags::CONCEPT_NAME).push(name);
        child.insert_sequence(tags::MEASURED_VALUE_SEQUENCE);
        root.add_child(ContentItem::from_dataset(child));
        report_with(root).validate().unwrap();

        let name = CodeItem::new("1", "TEST").with_meaning("Region");
        let mut bad_spatial = ContentItem::scoord(
            name.clone(),
            Relationship::Contains,
            SpatialCoordinates::new(GraphicType::Point, &[(1.0, 1.0)]).unwrap(),
        )
        .into_dataset();
        bad_spatial.insert_or_update(crate::model::element::DicomElement::new_bytes(
            tags::GRAPHIC_DATA,
            Vr::Fl,
            std::sync::Arc::new(crate::model::buffer::MemoryBuffer::new(
                [f32::NAN.to_le_bytes(), 1f32.to_le_bytes()].concat(),
            )),
        ));
        let mut root = container();
        root.add_child(ContentItem::from_dataset(bad_spatial));
        assert!(report_with(root).validate().is_err());
        let mut root = container();
        root.add_child(ContentItem::uid_ref(
            name,
            Relationship::Contains,
            "not-a-uid",
        ));
        assert!(report_with(root).validate().is_err());
        assert!(TemporalCoordinates::new(
            TemporalRangeType::Point,
            TemporalRange::SamplePositions(vec![0])
        )
        .is_err());
    }

    #[test]
    fn verified_reports_need_complete_observer_attributes() {
        let mut report = report_with(container());
        report
            .set_completion_flag("COMPLETE")
            .set_verification_flag("VERIFIED");
        let mut observer = Dataset::new();
        observer.insert_str(Tag::VERIFYING_OBSERVER_NAME, Vr::Pn, "Doe^Jane");
        observer.insert_str(Tag::VERIFYING_ORGANIZATION, Vr::Lo, "Hospital");
        observer.insert_str(Tag::VERIFICATION_DATE_TIME, Vr::Dt, "20261030120000");
        observer.insert_sequence(Tag::VERIFYING_OBSERVER_IDENTIFICATION_CODE_SEQUENCE);
        report
            .file
            .dataset
            .insert_sequence(Tag::VERIFYING_OBSERVER_SEQUENCE)
            .push(observer.clone());
        report.validate().unwrap();
        observer.remove(Tag::VERIFYING_ORGANIZATION);
        report.file.dataset.remove(Tag::VERIFYING_OBSERVER_SEQUENCE);
        report
            .file
            .dataset
            .insert_sequence(Tag::VERIFYING_OBSERVER_SEQUENCE)
            .push(observer);
        assert!(report.validate().is_err());
    }

    #[test]
    fn invalid_type_two_values_and_by_reference_targets_are_rejected() {
        let mut report = report_with(container());
        report
            .file
            .dataset
            .insert_str(Tag::PATIENT_BIRTH_DATE, Vr::Da, "not-a-date");
        assert!(report.validate().is_err());
        for target in [vec![], vec![0u32], vec![1, 9], vec![1, 1]] {
            let mut child = Dataset::new();
            child.insert_str(tags::RELATIONSHIP_TYPE, Vr::Cs, "INFERRED FROM");
            child.insert_or_update(crate::model::element::DicomElement::new_bytes(
                tags::REFERENCED_CONTENT_ITEM_IDENTIFIER,
                Vr::Ul,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::new(
                    target
                        .iter()
                        .flat_map(|n| n.to_le_bytes())
                        .collect::<Vec<_>>(),
                )),
            ));
            let mut root = container();
            root.add_child(ContentItem::from_dataset(child));
            assert!(
                report_with(root).validate().is_err(),
                "invalid target {target:?}"
            );
        }
    }

    #[test]
    fn imported_vrs_and_byte_backed_values_are_checked() {
        for tag in [
            tags::VALUE_TYPE,
            tags::COMPLETION_FLAG,
            tags::VERIFICATION_FLAG,
            tags::CONTINUITY_OF_CONTENT,
        ] {
            let mut report = report_with(container());
            let value = report.dataset().get_str(&tag).unwrap();
            report.file.dataset.insert_str(tag, Vr::Lo, value);
            assert!(report.validate().is_err());
        }
        let mut report = report_with(container());
        report
            .file
            .dataset
            .insert_str(tags::CONTENT_SEQUENCE, Vr::Lo, "bad");
        assert!(report.validate().is_err());
        let mut report = report_with(container());
        report
            .file
            .dataset
            .insert_sequence(Tag::PERFORMED_PROCEDURE_CODE_SEQUENCE)
            .push(Dataset::new());
        assert!(report.validate().is_err());
        for (text, valid) in [("x", false), ("20240101", true), ("", true)] {
            let mut report = report_with(container());
            report
                .file
                .dataset
                .insert_or_update(crate::model::element::DicomElement::new_bytes(
                    Tag::PATIENT_BIRTH_DATE,
                    Vr::Da,
                    std::sync::Arc::new(crate::model::buffer::MemoryBuffer::new(
                        text.as_bytes().to_vec(),
                    )),
                ));
            assert_eq!(
                report.validate().is_ok(),
                valid,
                "byte-backed date {text:?}"
            );
        }
        let mut report = report_with(container());
        report
            .file
            .dataset
            .insert_or_update(crate::model::element::DicomElement::new_bytes(
                Tag::CONTENT_DATE,
                Vr::Da,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::new(
                    b"20240101".to_vec(),
                )),
            ));
        report.validate().unwrap();
        let mut child = ContentItem::scoord(
            CodeItem::new("1", "TEST").with_meaning("Point"),
            Relationship::Contains,
            SpatialCoordinates::new(GraphicType::Point, &[(1.0, 1.0)]).unwrap(),
        )
        .into_dataset();
        child.insert_str(tags::GRAPHIC_DATA, Vr::Ds, "1\\1");
        let mut root = container();
        root.add_child(ContentItem::from_dataset(child));
        assert!(report_with(root).validate().is_err());
    }

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
        let mut dataset = Dataset::new();
        for (tag, vr, value) in [
            (Tag::CONTENT_DATE, Vr::Da, "20261030"),
            (Tag::CONTENT_TIME, Vr::Tm, "120000"),
            (Tag::INSTANCE_NUMBER, Vr::Is, "1"),
            (Tag::STUDY_INSTANCE_UID, Vr::Ui, "1.2.3"),
            (Tag::SERIES_INSTANCE_UID, Vr::Ui, "1.2.4"),
            (Tag::MODALITY, Vr::Cs, "SR"),
            (Tag::SERIES_NUMBER, Vr::Is, "1"),
            (Tag::PATIENT_NAME, Vr::Pn, ""),
            (Tag::PATIENT_ID, Vr::Lo, ""),
            (Tag::PATIENT_BIRTH_DATE, Vr::Da, ""),
            (Tag::PATIENT_SEX, Vr::Cs, ""),
            (Tag::STUDY_DATE, Vr::Da, ""),
            (Tag::STUDY_TIME, Vr::Tm, ""),
            (Tag::REFERRING_PHYSICIAN_NAME, Vr::Pn, ""),
            (Tag::STUDY_ID, Vr::Sh, ""),
            (Tag::ACCESSION_NUMBER, Vr::Sh, ""),
            (Tag::MANUFACTURER, Vr::Lo, ""),
        ] {
            dataset.insert_str(tag, vr, value);
        }
        dataset.insert_sequence(Tag::PERFORMED_PROCEDURE_CODE_SEQUENCE);
        dataset.insert_sequence(Tag::REFERENCED_PERFORMED_PROCEDURE_STEP_SEQUENCE);
        let mut report = StructuredReport::from_dataset(dataset);
        report.set_completion_flag("PARTIAL");
        report.set_verification_flag("UNVERIFIED");
        report.set_root(root);
        report
    }

    #[test]
    fn a_report_round_trips_through_bytes() {
        let mut root = container();
        root.add_child(ContentItem::text(
            CodeItem::new("121071", "DCM").with_meaning("Finding"),
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
            CodeItem::new("121071", "DCM").with_meaning("Finding"),
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
            CodeItem::new("121071", "DCM").with_meaning("Finding"),
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
            CodeItem::new("121071", "DCM").with_meaning("Finding"),
            None,
            None,
        ));
        let report = report_with(root);
        let error = report.validate().unwrap_err().to_string();
        assert!(error.contains("RelationshipType"), "got {error}");
    }

    #[test]
    fn a_scoord_with_the_wrong_point_count_fails_validation() {
        // a POLYLINE with two points, then relabelled POINT (which needs exactly one)
        let item = ContentItem::scoord(
            CodeItem::new("111030", "DCM").with_meaning("Region"),
            Relationship::Contains,
            SpatialCoordinates::new(GraphicType::Polyline, &[(0.0, 0.0), (1.0, 1.0)]).unwrap(),
        );
        let mut dataset = item.into_dataset();
        dataset.insert_str(tags::GRAPHIC_TYPE, Vr::Cs, "POINT");
        let mut root = container();
        root.add_child(ContentItem::from_dataset(dataset));
        let report = report_with(root);
        let error = report.validate().unwrap_err().to_string();
        assert!(
            error.contains("points"),
            "the point count is checked against the GraphicType, got {error}"
        );
    }

    #[test]
    fn an_item_without_its_value_tag_fails_validation() {
        let mut root = container();
        // a TEXT item whose TextValue was dropped
        let mut child = ContentItem::text(
            CodeItem::new("121071", "DCM").with_meaning("Finding"),
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
        let name = CodeItem::new("121071", "DCM").with_meaning("Finding");
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
            CodeItem::new("121071", "DCM").with_meaning("Finding"),
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
            CodeItem::new("121071", "DCM").with_meaning("Finding"),
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
        let name = CodeItem::new("121071", "DCM").with_meaning("Finding");
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
