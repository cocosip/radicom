//! Structured Report document (SR-01/02; docs/08-serialization-media.md §4.3).
//!
//! A `StructuredReport` wraps a `DicomFile` whose dataset holds the SR content
//! tree. `open` reads a file, `save` writes it with explicit-length sequences,
//! matching fo-dicom's behaviour (§4.3).

use crate::error::Error;
use crate::io::file::DicomFile;
use crate::io::writer::WriteOptions;
use crate::model::dataset::Dataset;
use crate::sr::content_item::{tags, ContentItem, ValueType};
use std::path::Path;

/// A structured report document (§4.3).
#[derive(Debug, Clone)]
pub struct StructuredReport {
    file: DicomFile,
}

impl StructuredReport {
    /// Wraps an already-built `DicomFile`.
    pub fn from_file(file: DicomFile) -> Self {
        StructuredReport { file }
    }

    /// Wraps a dataset in a minimal SR file (§4.3).
    pub fn from_dataset(dataset: Dataset) -> Self {
        StructuredReport { file: DicomFile::from_dataset(dataset) }
    }

    /// Opens an SR from a file (§4.3).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(StructuredReport { file: DicomFile::open(path)? })
    }

    /// Reads an SR from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        Ok(StructuredReport { file: DicomFile::from_bytes(bytes, &crate::io::file::ReadOptions::default())? })
    }

    /// The underlying file.
    pub fn file(&self) -> &DicomFile {
        &self.file
    }

    /// The document dataset.
    pub fn dataset(&self) -> &Dataset {
        &self.file.dataset
    }

    /// The root content items: the (0040,A730) ContentSequence at top level (§4.1).
    ///
    /// A conformant SR has exactly one CONTAINER root; this returns every item
    /// so malformed reports stay inspectable.
    pub fn root_items(&self) -> Vec<ContentItem> {
        self.file
            .dataset
            .try_get_sequence(tags::CONTENT_SEQUENCE)
            .map(|sequence| {
                sequence.items().iter().map(|item| ContentItem::from_dataset(item.clone())).collect()
            })
            .unwrap_or_default()
    }

    /// Writes the content tree back into the dataset from these root items.
    pub fn set_root_items(&mut self, items: Vec<ContentItem>) -> &mut Self {
        let sequence = self.file.dataset.insert_sequence(tags::CONTENT_SEQUENCE);
        sequence.items_mut().clear();
        for item in items {
            sequence.push(item.into_dataset());
        }
        self
    }

    /// The value type of the single root container, when present (§4.1).
    pub fn root_value_type(&self) -> Option<ValueType> {
        self.root_items().first().and_then(|item| item.value_type())
    }

    /// Validates that the top level is a single CONTAINER root (§4.1).
    pub fn validate(&self) -> Result<(), Error> {
        let roots = self.root_items();
        if roots.len() != 1 {
            return Err(Error::Data(crate::error::DataError::Other {
                message: format!("an SR must have exactly one root item, found {}", roots.len()),
            }));
        }
        if roots[0].value_type() != Some(ValueType::Container) {
            return Err(Error::Data(crate::error::DataError::Other {
                message: "the SR root must be a CONTAINER".into(),
            }));
        }
        Ok(())
    }

    /// Saves with explicit-length sequences/items (§4.3, fo-dicom behaviour).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        let options = WriteOptions {
            explicit_length_sequences: true,
            explicit_length_sequence_items: true,
            keep_group_lengths: false,
        };
        let bytes = self.file.to_bytes(&options)?;
        std::fs::write(path, bytes)
            .map_err(|e| Error::File(crate::error::FileError::Other(format!("write: {e}"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sr::code_item::CodeItem;
    use crate::sr::content_item::Relationship;

    fn container() -> ContentItem {
        ContentItem::container(
            CodeItem::new("126000", "DCM").with_meaning("Imaging Measurement Report"),
            None,
            Some("SEPARATE"),
        )
    }

    #[test]
    fn a_report_round_trips_through_bytes() {
        let mut root = container();
        root.add_child(ContentItem::text(
            CodeItem::new("121071", "DCM"),
            Relationship::Contains,
            "Mass in the left lung",
        ));

        let mut report = StructuredReport::from_dataset(Dataset::new());
        report.set_root_items(vec![root]);
        report.validate().unwrap();
        assert_eq!(report.root_value_type(), Some(ValueType::Container));

        let bytes = {
            let options = WriteOptions {
                explicit_length_sequences: true,
                explicit_length_sequence_items: true,
                keep_group_lengths: false,
            };
            report.file().to_bytes(&options).unwrap()
        };
        let reopened = StructuredReport::from_bytes(&bytes).unwrap();
        reopened.validate().unwrap();
        let roots = reopened.root_items();
        assert_eq!(roots.len(), 1);
        let children = roots[0].children();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].text_value().as_deref(), Some("Mass in the left lung"));
    }

    #[test]
    fn a_report_with_no_root_fails_validation() {
        let report = StructuredReport::from_dataset(Dataset::new());
        assert!(report.validate().is_err());
        assert_eq!(report.root_value_type(), None);
    }

    #[test]
    fn a_text_root_fails_validation() {
        let mut report = StructuredReport::from_dataset(Dataset::new());
        report.set_root_items(vec![ContentItem::text(
            CodeItem::new("121071", "DCM"),
            Relationship::Contains,
            "not a container",
        )]);
        assert!(report.validate().is_err(), "a TEXT root is not a valid SR");
    }

    #[test]
    fn save_writes_a_file_that_opens_back() {
        let mut root = container();
        root.add_child(ContentItem::text(CodeItem::new("121071", "DCM"), Relationship::Contains, "ok"));
        let mut report = StructuredReport::from_dataset(Dataset::new());
        report.set_root_items(vec![root]);

        let path = std::env::temp_dir().join(format!("radicom-sr-{}.dcm", std::process::id()));
        report.save(&path).unwrap();
        let reopened = StructuredReport::open(&path).unwrap();
        reopened.validate().unwrap();
        assert_eq!(reopened.root_items()[0].children().len(), 1);
        let _ = std::fs::remove_file(path);
    }
}
