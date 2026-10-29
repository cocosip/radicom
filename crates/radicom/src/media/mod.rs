//! DICOMDIR — the Media Storage Directory (MEDIA-01; docs/01-architecture.md §12).
//!
//! A DICOMDIR is a normal DICOM file whose dataset holds a flat
//! `DirectoryRecordSequence` (0004,1220). The hierarchy is expressed by record
//! offsets: `OffsetOfNextDirectoryRecord` (0004,1400) links the next sibling,
//! `OffsetOfReferencedLowerLevelDirectoryEntity` (0004,1420) the first child,
//! and `OffsetOfFirstRootDirectoryRecord` (0004,1200) the first root record.
//!
//! Writing uses the two-pass scheme from §3.3: every record's encoded size is
//! measured with the writer's length calculator, offsets are derived from the
//! running total, the links are patched, and only then is the file serialized.

use crate::error::{Error, FileError};
use crate::io::file::{DicomFile, FileFormat, ReadOptions};
use crate::io::writer::{DicomWriter, WriteOptions};
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::model::transfer_syntax::syntaxes;
use crate::model::vr::Vr;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub mod icon;
pub mod scanner;
pub use icon::{attach_icon, BasicIconGenerator, IconGenerator};
pub use scanner::{
    scan_directory, scan_directory_with_control, DicomDirectoryBuilder, Glob, ScanControl, ScanErrorPolicy, ScanEvent,
    ScanOptions, ScanSummary, ScanVisitor,
};

/// Tags of the media storage directory (§3.1, §3.2).
pub mod tags {
    use crate::model::tag::Tag;

    /// (0004,1130) File-set ID.
    pub const FILE_SET_ID: Tag = Tag::FILE_SET_ID;
    /// (0004,1200) Offset of the first root directory record.
    pub const OFFSET_FIRST_ROOT: Tag = Tag::OFFSET_OF_THE_FIRST_DIRECTORY_RECORD_OF_THE_ROOT_DIRECTORY_ENTITY;
    /// (0004,1202) Offset of the last root directory record.
    pub const OFFSET_LAST_ROOT: Tag = Tag::OFFSET_OF_THE_LAST_DIRECTORY_RECORD_OF_THE_ROOT_DIRECTORY_ENTITY;
    /// (0004,1212) File-set consistency flag.
    pub const FILE_SET_CONSISTENCY_FLAG: Tag = Tag::FILE_SET_CONSISTENCY_FLAG;
    /// (0004,1220) Directory record sequence.
    pub const DIR_RECORD_SEQUENCE: Tag = Tag::DIRECTORY_RECORD_SEQUENCE;
    /// (0004,1400) Offset of the next directory record.
    pub const OFFSET_NEXT: Tag = Tag::OFFSET_OF_THE_NEXT_DIRECTORY_RECORD;
    /// (0004,1410) Record in-use flag.
    pub const RECORD_IN_USE_FLAG: Tag = Tag::RECORD_IN_USE_FLAG;
    /// (0004,1420) Offset of the referenced lower level entity.
    pub const OFFSET_LOWER: Tag = Tag::OFFSET_OF_REFERENCED_LOWER_LEVEL_DIRECTORY_ENTITY;
    /// (0004,1430) Directory record type.
    pub const RECORD_TYPE: Tag = Tag::DIRECTORY_RECORD_TYPE;
    /// (0004,1500) Referenced file ID.
    pub const REFERENCED_FILE_ID: Tag = Tag::REFERENCED_FILE_ID;
    /// (0004,1510) Referenced SOP class UID in file.
    pub const REFERENCED_SOP_CLASS: Tag = Tag::REFERENCED_SOP_CLASS_UID_IN_FILE;
    /// (0004,1511) Referenced SOP instance UID in file.
    pub const REFERENCED_SOP_INSTANCE: Tag = Tag::REFERENCED_SOP_INSTANCE_UID_IN_FILE;
    /// (0004,1512) Referenced transfer syntax UID in file.
    pub const REFERENCED_TRANSFER_SYNTAX: Tag = Tag::REFERENCED_TRANSFER_SYNTAX_UID_IN_FILE;

    /// (0010,0010) Patient's Name.
    pub const PATIENT_NAME: Tag = Tag::PATIENT_NAME;
    /// (0010,0020) Patient ID.
    pub const PATIENT_ID: Tag = Tag::PATIENT_ID;
    /// (0020,000D) Study Instance UID.
    pub const STUDY_INSTANCE_UID: Tag = Tag::STUDY_INSTANCE_UID;
    /// (0020,000E) Series Instance UID.
    pub const SERIES_INSTANCE_UID: Tag = Tag::SERIES_INSTANCE_UID;
    /// (0008,0016) SOP Class UID.
    pub const SOP_CLASS_UID: Tag = Tag::SOP_CLASS_UID;
    /// (0008,0018) SOP Instance UID.
    pub const SOP_INSTANCE_UID: Tag = Tag::SOP_INSTANCE_UID;
}

/// The SOP class of a media storage directory (§3.1).
pub const MEDIA_STORAGE_DIRECTORY_SOP_CLASS: &str = "1.2.840.10008.1.3.10";

/// Directory record types recognised at the leaf level (§3.1, PS3.3 F.5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordType {
    Patient,
    Study,
    Series,
    Image,
    SrDocument,
    Presentation,
    /// Any other record type, kept verbatim so unknown trees round-trip.
    Other(String),
}

impl RecordType {
    pub fn as_str(&self) -> &str {
        match self {
            RecordType::Patient => "PATIENT",
            RecordType::Study => "STUDY",
            RecordType::Series => "SERIES",
            RecordType::Image => "IMAGE",
            RecordType::SrDocument => "SR DOCUMENT",
            RecordType::Presentation => "PRESENTATION",
            RecordType::Other(raw) => raw.as_str(),
        }
    }

    pub fn parse(text: &str) -> Self {
        match text.trim() {
            "PATIENT" => RecordType::Patient,
            "STUDY" => RecordType::Study,
            "SERIES" => RecordType::Series,
            "IMAGE" => RecordType::Image,
            "SR DOCUMENT" => RecordType::SrDocument,
            "PRESENTATION" => RecordType::Presentation,
            other => RecordType::Other(other.to_string()),
        }
    }
}

/// The storage categories a leaf record can index (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageCategory {
    Image,
    SrDocument,
    Presentation,
}

impl StorageCategory {
    /// Maps a file's SOP class to the record type that indexes it (§3.4).
    pub fn of_sop_class(sop_class: &str) -> StorageCategory {
        match sop_class.trim() {
            // Basic/Enhanced/Comprehensive SR, Comprehensive 3D SR, Mammography CAD SR,
            // Key Object Selection
            "1.2.840.10008.5.1.4.1.1.88.11"
            | "1.2.840.10008.5.1.4.1.1.88.22"
            | "1.2.840.10008.5.1.4.1.1.88.33"
            | "1.2.840.10008.5.1.4.1.1.88.34"
            | "1.2.840.10008.5.1.4.1.1.88.50"
            | "1.2.840.10008.5.1.4.1.1.88.59" => StorageCategory::SrDocument,
            // Grayscale/Color/Pseudo-Color/Blending/XA softcopy presentation states
            "1.2.840.10008.5.1.4.1.1.11.1"
            | "1.2.840.10008.5.1.4.1.1.11.2"
            | "1.2.840.10008.5.1.4.1.1.11.3"
            | "1.2.840.10008.5.1.4.1.1.11.4"
            | "1.2.840.10008.5.1.4.1.1.11.5" => StorageCategory::Presentation,
            _ => StorageCategory::Image,
        }
    }

    pub fn record_type(self) -> RecordType {
        match self {
            StorageCategory::Image => RecordType::Image,
            StorageCategory::SrDocument => RecordType::SrDocument,
            StorageCategory::Presentation => RecordType::Presentation,
        }
    }
}

/// One directory record, plus the parent the tree view uses (§3.2).
#[derive(Debug, Clone)]
pub struct DirectoryRecord {
    pub dataset: Dataset,
    /// Parent record index; `None` for a root record.
    pub parent: Option<usize>,
}

impl DirectoryRecord {
    pub fn record_type(&self) -> RecordType {
        self.dataset
            .try_get_str(tags::RECORD_TYPE)
            .map(|s| RecordType::parse(&s))
            .unwrap_or_else(|| RecordType::Other(String::new()))
    }

    /// `(0004,1500)` as a path; the stored form is `\`-separated.
    pub fn referenced_file_id(&self) -> Option<PathBuf> {
        let id = self.dataset.try_get_str(tags::REFERENCED_FILE_ID)?;
        let mut path = PathBuf::new();
        for part in id.split('\\').filter(|part| !part.is_empty()) {
            path.push(part);
        }
        Some(path)
    }

    pub fn sop_class_uid(&self) -> Option<String> {
        trimmed(&self.dataset, tags::REFERENCED_SOP_CLASS)
    }

    pub fn sop_instance_uid(&self) -> Option<String> {
        trimmed(&self.dataset, tags::REFERENCED_SOP_INSTANCE)
    }

    pub fn transfer_syntax_uid(&self) -> Option<String> {
        trimmed(&self.dataset, tags::REFERENCED_TRANSFER_SYNTAX)
    }

    pub fn patient_id(&self) -> Option<String> {
        trimmed(&self.dataset, tags::PATIENT_ID)
    }

    pub fn study_uid(&self) -> Option<String> {
        trimmed(&self.dataset, tags::STUDY_INSTANCE_UID)
    }

    pub fn series_uid(&self) -> Option<String> {
        trimmed(&self.dataset, tags::SERIES_INSTANCE_UID)
    }
}

impl std::fmt::Display for DirectoryRecord {
    /// fo-dicom `DicomDirectoryRecord.ToString()`: the record dataset dump.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.dataset.to_dump_string())
    }
}

fn trimmed(dataset: &Dataset, tag: Tag) -> Option<String> {
    dataset.try_get_str(tag).map(|s| s.trim().to_string())
}

/// The four records associated with one file added to a DICOMDIR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DicomDirectoryEntry {
    /// Patient record index.
    pub patient: usize,
    /// Study record index.
    pub study: usize,
    /// Series record index.
    pub series: usize,
    /// Image/SR/presentation record index.
    pub instance: usize,
}

/// A media storage directory: the file plus its record list (§3.1).
///
/// Records are stored in the order they appear in the sequence; `parent`
/// carries the hierarchy, so sibling order is the order of insertion.
#[derive(Debug, Clone)]
pub struct DicomDirectory {
    file: DicomFile,
    records: Vec<DirectoryRecord>,
    roots: Vec<usize>,
}

impl DicomDirectory {
    /// Creates an empty directory with `file_set_id` (§3.3 step 3).
    pub fn new(file_set_id: impl Into<String>) -> Self {
        Self::try_new(file_set_id).expect("DICOMDIR FileSetID must not be empty")
    }

    /// Creates an empty directory, returning an error for an invalid FileSetID.
    pub fn try_new(file_set_id: impl Into<String>) -> Result<Self, Error> {
        let file_set_id = file_set_id.into();
        if file_set_id.is_empty() {
            return Err(record_error("DICOMDIR FileSetID must not be empty"));
        }
        let mut dataset = Dataset::new();
        dataset.insert_str(tags::FILE_SET_ID, Vr::Sh, file_set_id);
        dataset.insert_value(tags::FILE_SET_CONSISTENCY_FLAG, 1u16);
        // a DICOMDIR carries its own SOP Instance UID; setting it once keeps the
        // file identity (and thus every record offset) stable across re-serialisation
        dataset.insert_str(tags::SOP_INSTANCE_UID, Vr::Ui, crate::model::uid::Uid::generate_value());
        dataset.insert_sequence(tags::DIR_RECORD_SEQUENCE);
        dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());

        let mut file_meta = Dataset::new();
        file_meta.insert_str(Tag::MEDIA_STORAGE_SOP_CLASS_UID, Vr::Ui, MEDIA_STORAGE_DIRECTORY_SOP_CLASS);
        let mut file = DicomFile::from_dataset(dataset);
        file.file_meta = file_meta;
        file.format = FileFormat::Dicom3;

        Ok(DicomDirectory { file, records: Vec::new(), roots: Vec::new() })
    }

    /// Opens and stitches an existing DICOMDIR (§3.2).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)
            .map_err(|e| Error::File(FileError::Other(format!("{}: {e}", path.display()))))?;
        Self::from_bytes(&bytes)
    }

    /// Parses a DICOMDIR from bytes (§3.2).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let file = DicomFile::from_bytes(bytes, &ReadOptions::default())?;
        let (records, roots) = stitch(bytes, &file.dataset);
        Ok(DicomDirectory { file, records, roots })
    }

    pub fn file(&self) -> &DicomFile {
        &self.file
    }

    pub fn records(&self) -> &[DirectoryRecord] {
        &self.records
    }

    /// Indexes of the root records, in sibling order.
    pub fn roots(&self) -> &[usize] {
        &self.roots
    }

    pub fn file_set_id(&self) -> Option<String> {
        trimmed(&self.file.dataset, tags::FILE_SET_ID)
    }

    /// Direct children of `index`, in sibling order (§3.2).
    pub fn children(&self, index: usize) -> Vec<usize> {
        (0..self.records.len())
            .filter(|i| self.records[*i].parent == Some(index))
            .collect()
    }

    /// Every record in depth-first order with its depth (§3.2).
    ///
    /// A corrupt directory whose offset links form a cycle is walked safely
    /// (each record is emitted at most once) instead of recursing forever.
    pub fn walk(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut visited = vec![false; self.records.len()];
        for root in &self.roots {
            self.walk_from(*root, 0, &mut out, &mut visited);
        }
        out
    }

    fn walk_from(&self, index: usize, depth: usize, out: &mut Vec<(usize, usize)>, visited: &mut [bool]) {
        if index >= self.records.len() || visited[index] {
            return;
        }
        visited[index] = true;
        out.push((index, depth));
        for child in self.children(index) {
            self.walk_from(child, depth + 1, out, visited);
        }
    }

    /// Validates the record tree (§3.2): a non-empty FileSetID, every
    /// parent/root index in range, roots without a parent, no cycles, and no
    /// record unreachable from a root. A DICOMDIR parsed from untrusted bytes
    /// can violate any of these.
    pub fn validate(&self) -> Result<(), Error> {
        if self.file_set_id().is_none_or(|id| id.trim().is_empty()) {
            return Err(record_error("DICOMDIR FileSetID must not be empty"));
        }
        let count = self.records.len();
        for (index, record) in self.records.iter().enumerate() {
            if let Some(parent) = record.parent {
                if parent == index || parent >= count {
                    return Err(record_error(format!("record {index} has an invalid parent {parent}")));
                }
            }
        }
        for &root in &self.roots {
            if root >= count {
                return Err(record_error(format!("root offset {root} is out of range")));
            }
            if self.records[root].parent.is_some() {
                return Err(record_error(format!("root record {root} has a parent")));
            }
        }
        // 0 = unvisited, 1 = on the current path, 2 = finished
        let mut state = vec![0u8; count];
        for &root in &self.roots {
            self.validate_visit(root, &mut state)?;
        }
        if state.contains(&0) {
            return Err(record_error("a record is not reachable from any root"));
        }
        Ok(())
    }

    fn validate_visit(&self, index: usize, state: &mut [u8]) -> Result<(), Error> {
        match state[index] {
            1 => return Err(record_error("DICOMDIR record tree contains a cycle")),
            2 => return Ok(()),
            _ => {}
        }
        state[index] = 1;
        for child in self.children(index) {
            self.validate_visit(child, state)?;
        }
        state[index] = 2;
        Ok(())
    }

    /// The first record matching `kind` and `key`, searching level records.
    pub fn find_patient(&self, patient_id: &str) -> Option<usize> {
        self.records.iter().position(|record| {
            record.record_type() == RecordType::Patient && record.patient_id().as_deref() == Some(patient_id)
        })
    }

    pub fn find_study(&self, study_uid: &str) -> Option<usize> {
        self.records.iter().position(|record| {
            record.record_type() == RecordType::Study && record.study_uid().as_deref() == Some(study_uid)
        })
    }

    pub fn find_series(&self, series_uid: &str) -> Option<usize> {
        self.records.iter().position(|record| {
            record.record_type() == RecordType::Series && record.series_uid().as_deref() == Some(series_uid)
        })
    }

    /// Adds `file` to the hierarchy under `referenced_file_id` (§3.4).
    ///
    /// The PATIENT, STUDY and SERIES records are found by key or created, then
    /// the leaf record is appended with its `(0004,1510..1512)` references.
    pub fn add_file(
        &mut self,
        file: &DicomFile,
        referenced_file_id: impl AsRef<Path>,
    ) -> Result<&mut Self, Error> {
        self.add_file_inner(file, referenced_file_id.as_ref(), None)?;
        Ok(self)
    }

    /// Adds `file` and returns the patient, study, series and leaf record indices.
    pub fn add_file_entry(
        &mut self,
        file: &DicomFile,
        referenced_file_id: impl AsRef<Path>,
    ) -> Result<DicomDirectoryEntry, Error> {
        self.add_file_inner(file, referenced_file_id.as_ref(), None)
    }

    /// Like [`add_file`](Self::add_file) but also attaches an icon built by
    /// `generator` as `(0088,0200) IconImageSequence` (§3.4, MEDIA-02).
    pub fn add_file_with_icon(
        &mut self,
        file: &DicomFile,
        referenced_file_id: impl AsRef<Path>,
        generator: &dyn IconGenerator,
    ) -> Result<&mut Self, Error> {
        self.add_file_inner(file, referenced_file_id.as_ref(), Some(generator))?;
        Ok(self)
    }

    /// Like [`add_file_entry`](Self::add_file_entry), also attaching an icon.
    pub fn add_file_entry_with_icon(
        &mut self,
        file: &DicomFile,
        referenced_file_id: impl AsRef<Path>,
        generator: &dyn IconGenerator,
    ) -> Result<DicomDirectoryEntry, Error> {
        self.add_file_inner(file, referenced_file_id.as_ref(), Some(generator))
    }

    fn add_file_inner(
        &mut self,
        file: &DicomFile,
        referenced_file_id: &Path,
        generator: Option<&dyn IconGenerator>,
    ) -> Result<DicomDirectoryEntry, Error> {
        let dataset = &file.dataset;
        let patient_id = dataset.try_get_str(tags::PATIENT_ID).unwrap_or_default();
        let study_uid = required(dataset, tags::STUDY_INSTANCE_UID, "Study Instance UID")?;
        let series_uid = required(dataset, tags::SERIES_INSTANCE_UID, "Series Instance UID")?;
        let sop_class = required(dataset, tags::SOP_CLASS_UID, "SOP Class UID")?;
        let sop_instance = required(dataset, tags::SOP_INSTANCE_UID, "SOP Instance UID")?;

        let patient = match self.find_patient(&patient_id) {
            Some(index) => index,
            None => {
                let mut record = level_record(RecordType::Patient, dataset);
                record.dataset.insert_str(tags::PATIENT_ID, Vr::Lo, patient_id.clone());
                self.push_root(record)
            }
        };

        let study = self.find_or_create_child(patient, RecordType::Study, &study_uid, dataset);
        let series = self.find_or_create_child(study, RecordType::Series, &series_uid, dataset);

        let mut record = Dataset::new();
        record.insert_str(tags::RECORD_TYPE, Vr::Cs, StorageCategory::of_sop_class(&sop_class).record_type().as_str());
        record.insert_value(tags::RECORD_IN_USE_FLAG, 0xFFFFu16);
        record.insert_str(tags::REFERENCED_SOP_CLASS, Vr::Ui, sop_class);
        record.insert_str(tags::REFERENCED_SOP_INSTANCE, Vr::Ui, sop_instance);
        record.insert_str(tags::REFERENCED_TRANSFER_SYNTAX, Vr::Ui, dataset.transfer_syntax().uid());
        record.insert_str(tags::REFERENCED_FILE_ID, Vr::Cs, path_to_file_id(referenced_file_id));

        // optional preview (§3.4): built from a clone so the source is untouched.
        // A generator failure only costs the preview — fo-dicom's
        // AddIconSequence swallows every exception, because a missing icon must
        // never abort a directory build.
        if let Some(generator) = generator {
            let mut source = dataset.clone();
            if let Ok(Some(icon)) = generator.generate(&mut source) {
                attach_icon(&mut record, icon);
            }
        }

        let instance = self.records.len();
        self.records.push(DirectoryRecord { dataset: record, parent: Some(series) });
        Ok(DicomDirectoryEntry { patient, study, series, instance })
    }

    /// Finds a child of `parent` with matching type and UID, else creates one.
    fn find_or_create_child(
        &mut self,
        parent: usize,
        kind: RecordType,
        uid: &str,
        source: &Dataset,
    ) -> usize {
        let existing = self.children(parent).into_iter().find(|index| {
            let record = &self.records[*index];
            if record.record_type() != kind {
                return false;
            }
            match kind {
                RecordType::Study => record.study_uid().as_deref() == Some(uid),
                RecordType::Series => record.series_uid().as_deref() == Some(uid),
                _ => false,
            }
        });
        if let Some(index) = existing {
            return index;
        }
        let mut record = level_record(kind.clone(), source);
        let uid_tag = match kind {
            RecordType::Series => tags::SERIES_INSTANCE_UID,
            _ => tags::STUDY_INSTANCE_UID,
        };
        record.dataset.insert_str(uid_tag, Vr::Ui, uid.to_string());
        let index = self.records.len();
        self.records.push(DirectoryRecord { dataset: record.dataset, parent: Some(parent) });
        index
    }

    fn push_root(&mut self, mut record: DirectoryRecord) -> usize {
        record.parent = None;
        let index = self.records.len();
        self.records.push(record);
        self.roots.push(index);
        index
    }

    /// Computes offsets, then fills them into the already-built sequence (§3.3).
    ///
    /// PS3.3 F.2.2: every offset is an **absolute byte position in the file**.
    /// One write with placeholder offsets fixes every element width (offsets are
    /// fixed-width ULs), so the measured positions can be patched in place
    /// instead of rebuilding the whole sequence a second time (review §3.5).
    ///
    /// Fills the record sequence once with placeholder offsets, measures the
    /// resulting layout, then patches the offset values in place. One record
    /// clone instead of two full sequences (review §3.5).
    fn rebuild(&mut self) -> Result<(), Error> {
        let links: Vec<(Option<usize>, Option<usize>)> = (0..self.records.len())
            .map(|index| (self.next_sibling(index), self.children(index).first().copied()))
            .collect();

        let placeholders = vec![0u32; self.records.len()];
        self.write_layout(&links, &placeholders)?;
        let positions = self.record_positions()?;
        self.patch_offsets(&links, &positions);
        Ok(())
    }

    /// Rewrites the offset values already present in the record datasets and
    /// the sequence items built by [`Self::write_layout`].
    fn patch_offsets(&mut self, links: &[(Option<usize>, Option<usize>)], positions: &[u32]) {
        for (index, record) in self.records.iter_mut().enumerate() {
            record.dataset.remove(tags::OFFSET_NEXT);
            record.dataset.remove(tags::OFFSET_LOWER);
            if let Some(next) = links[index].0 {
                record.dataset.insert_value(tags::OFFSET_NEXT, positions[next]);
            }
            if let Some(child) = links[index].1 {
                record.dataset.insert_value(tags::OFFSET_LOWER, positions[child]);
            }
        }
        if let Some(sequence) = self.file.dataset.sequence_mut(tags::DIR_RECORD_SEQUENCE) {
            for (index, item) in sequence.items_mut().iter_mut().enumerate() {
                item.remove(tags::OFFSET_NEXT);
                item.remove(tags::OFFSET_LOWER);
                if let Some(next) = links[index].0 {
                    item.insert_value(tags::OFFSET_NEXT, positions[next]);
                }
                if let Some(child) = links[index].1 {
                    item.insert_value(tags::OFFSET_LOWER, positions[child]);
                }
            }
        }
        match self.roots.first().copied() {
            Some(index) => {
                self.file.dataset.insert_value(tags::OFFSET_FIRST_ROOT, positions[index]);
            }
            None => {
                self.file.dataset.remove(tags::OFFSET_FIRST_ROOT);
            }
        }
        match self.roots.last().copied() {
            Some(index) => {
                self.file.dataset.insert_value(tags::OFFSET_LAST_ROOT, positions[index]);
            }
            None => {
                self.file.dataset.remove(tags::OFFSET_LAST_ROOT);
            }
        }
    }

    /// Absolute byte position of every record item in the file about to be
    /// written (§3.3).
    ///
    /// The positions follow from the encoded layout — preamble, file meta, the
    /// elements ahead of `(0004,1220)`, the sequence header, then one item per
    /// record — rather than from scanning the serialized bytes. A scan cannot
    /// tell a record item from an item nested inside a record: an
    /// `IconImageSequence (0088,0200)` adds an item header and shifted every
    /// offset by one record.
    fn record_positions(&self) -> Result<Vec<u32>, Error> {
        let options = WriteOptions::default();
        let syntax = self.file.dataset.transfer_syntax();
        if syntax.is_deflate() {
            // PS3.10: the media storage directory is never compressed, and a
            // deflated payload has no byte position to point at
            return Err(record_error("a DICOMDIR cannot be deflated"));
        }
        let writer = DicomWriter::new(options.clone());

        // elements ordered ahead of the record sequence
        let mut ahead = Dataset::new();
        for item in self.file.dataset.items() {
            if item.tag() < &tags::DIR_RECORD_SEQUENCE {
                ahead.insert_or_update(item.clone());
            }
        }
        // preamble (128) + "DICM" (4) + file meta, computed without serializing
        // the whole dataset (which would copy every record and icon)
        let before_sequence = self.file.encoded_prefix_len(&options)?;
        // an SQ header is tag(4) + VR(2) + reserved(2) + length(4) when explicit
        let sequence_header = if syntax.is_explicit_vr() { 12 } else { 8 };
        let mut cursor = before_sequence + writer.dataset_length(&ahead, &syntax)? + sequence_header;

        // an item is an 8-byte header, the record dataset and — for the default
        // undefined-length items — an 8-byte item delimiter
        let item_delimiter = if options.explicit_length_sequence_items { 0 } else { 8 };
        let mut positions = Vec::with_capacity(self.records.len());
        for record in &self.records {
            positions.push(cursor as u32);
            cursor += 8 + writer.dataset_length(&record.dataset, &syntax)? + item_delimiter;
        }
        Ok(positions)
    }

    /// Writes the records, links and root offsets into the file dataset.
    fn write_layout(
        &mut self,
        links: &[(Option<usize>, Option<usize>)],
        positions: &[u32],
    ) -> Result<(), Error> {
        for (index, record) in self.records.iter_mut().enumerate() {
            record.dataset.remove(tags::OFFSET_NEXT);
            record.dataset.remove(tags::OFFSET_LOWER);
            if let Some(next) = links[index].0 {
                record.dataset.insert_value(tags::OFFSET_NEXT, positions[next]);
            }
            if let Some(child) = links[index].1 {
                record.dataset.insert_value(tags::OFFSET_LOWER, positions[child]);
            }
        }
        let items: Vec<Dataset> = self.records.iter().map(|record| record.dataset.clone()).collect();
        let sequence = self.file.dataset.insert_sequence(tags::DIR_RECORD_SEQUENCE);
        *sequence.items_mut() = items;

        match self.roots.first().copied() {
            Some(index) => {
                self.file.dataset.insert_value(tags::OFFSET_FIRST_ROOT, positions[index]);
            }
            None => {
                self.file.dataset.remove(tags::OFFSET_FIRST_ROOT);
            }
        }
        match self.roots.last().copied() {
            Some(index) => {
                self.file.dataset.insert_value(tags::OFFSET_LAST_ROOT, positions[index]);
            }
            None => {
                self.file.dataset.remove(tags::OFFSET_LAST_ROOT);
            }
        }
        Ok(())
    }

    /// The next record sharing this record's parent (§3.1).
    fn next_sibling(&self, index: usize) -> Option<usize> {
        let siblings = match self.records.get(index)?.parent {
            Some(parent) => self.children(parent),
            None => self.roots.clone(),
        };
        let position = siblings.iter().position(|i| *i == index)?;
        siblings.get(position + 1).copied()
    }

    /// Saves the directory, rebuilding the offsets first (§3.3).
    pub fn save(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        let bytes = self.to_bytes()?;
        std::fs::write(path, bytes)
            .map_err(|e| Error::File(FileError::Other(format!("write: {e}"))))
    }

    /// Serializes the directory, rebuilding the offsets first (§3.3).
    pub fn to_bytes(&mut self) -> Result<Vec<u8>, Error> {
        self.rebuild()?;
        self.file.to_bytes(&WriteOptions::default())
    }
}

/// A level record pre-populated with the identifying attributes of its level.
fn level_record(kind: RecordType, source: &Dataset) -> DirectoryRecord {
    let mut record = Dataset::new();
    record.insert_str(tags::RECORD_TYPE, Vr::Cs, kind.as_str());
    record.insert_value(tags::RECORD_IN_USE_FLAG, 0xFFFFu16);
    for (tag, vr) in identifying_attributes(&kind) {
        if !record.contains(tag) {
            if let Some(text) = source.try_get_str(tag) {
                record.insert_str(tag.clone(), *vr, text);
            }
        }
    }
    DirectoryRecord { dataset: record, parent: None }
}

/// Attributes copied from the file into a level record (§3.4).
fn identifying_attributes(kind: &RecordType) -> &'static [(Tag, Vr)] {
    const PATIENT: &[(Tag, Vr)] = &[(tags::PATIENT_NAME, Vr::Pn)];
    const STUDY: &[(Tag, Vr)] = &[
        (Tag::STUDY_DATE, Vr::Da), // Study Date
        (Tag::STUDY_TIME, Vr::Tm), // Study Time
        (Tag::ACCESSION_NUMBER, Vr::Sh), // Accession Number
        (Tag::STUDY_DESCRIPTION, Vr::Lo), // Study Description
        (Tag::STUDY_ID, Vr::Sh), // Study ID
    ];
    const SERIES: &[(Tag, Vr)] = &[
        (Tag::MODALITY, Vr::Cs), // Modality
        (Tag::SERIES_NUMBER, Vr::Is), // Series Number
    ];
    match kind {
        RecordType::Patient => PATIENT,
        RecordType::Study => STUDY,
        RecordType::Series => SERIES,
        _ => &[],
    }
}

fn required(dataset: &Dataset, tag: Tag, what: &str) -> Result<String, Error> {
    dataset
        .try_get_str(tag)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| record_error(format!("{what} is required for a directory record")))
}

/// `\`-separated file ID from a path (§3.4).
fn path_to_file_id(path: &Path) -> String {
    path.iter().filter_map(|part| part.to_str()).collect::<Vec<_>>().join("\\")
}

fn record_error(message: impl Into<String>) -> Error {
    Error::File(FileError::Other(message.into()))
}

/// Rebuilds the hierarchy from the record offsets (§3.2).
///
/// Offsets are byte positions inside the file, so the tree is stitched from
/// the positions the records actually occupy in `bytes`. The stored links are
/// `parent → child`; inverting them gives the tree view. A link that names an
/// unknown offset is ignored rather than failing the load, which keeps dirty
/// DICOMDIRs readable (§3.2 tolerance).
fn stitch(bytes: &[u8], dataset: &Dataset) -> (Vec<DirectoryRecord>, Vec<usize>) {
    let items: Vec<Dataset> = dataset
        .try_get_sequence(tags::DIR_RECORD_SEQUENCE)
        .map(|sequence| sequence.items().to_vec())
        .unwrap_or_default();
    if items.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // positions of the record items in the file; fall back to item order when
    // the raw bytes cannot be scanned (e.g. round-tripped through a dataset)
    let offsets = record_offsets(bytes, dataset, &items);
    let index_of = |offset: u32| offsets.iter().position(|o| *o == offset);
    let next_of = |index: usize| -> Option<usize> {
        items[index]
            .get::<u32>(tags::OFFSET_NEXT)
            .ok()
            .filter(|next| *next != 0)
            .and_then(index_of)
    };

    // The hierarchy is the sibling chain (next) rooted at each parent's first
    // child (lower) (§3.1). Walk it once from every offset that no other
    // record points at, which yields parent links and roots together.
    let mut parents: Vec<Option<usize>> = vec![None; items.len()];
    let pointed: Vec<bool> = {
        let mut pointed = vec![false; items.len()];
        for item in &items {
            if let Ok(lower) = item.get::<u32>(tags::OFFSET_LOWER) {
                if let Some(index) = index_of(lower) {
                    pointed[index] = true;
                }
            }
        }
        pointed
    };

    let mut roots = Vec::new();
    // a record is a root when nothing points at it via lower, and it is not
    // itself reached as a sibling (checked by walking the sibling chains)
    let mut reached = vec![false; items.len()];
    for (index, item) in items.iter().enumerate() {
        if let Ok(lower) = item.get::<u32>(tags::OFFSET_LOWER) {
            if let Some(first_child) = index_of(lower) {
                let mut cursor = Some(first_child);
                while let Some(child) = cursor {
                    if reached[child] {
                        break;
                    }
                    reached[child] = true;
                    parents[child] = Some(index);
                    cursor = next_of(child);
                }
            }
        }
    }

    // roots: the stored first-root chain, else every unreached record
    if let Ok(first) = dataset.get::<u32>(tags::OFFSET_FIRST_ROOT) {
        let mut cursor = index_of(first);
        while let Some(index) = cursor {
            if roots.contains(&index) {
                break;
            }
            roots.push(index);
            reached[index] = true;
            cursor = next_of(index);
        }
    }
    if roots.is_empty() {
        roots = (0..items.len()).filter(|i| !reached[*i] && !pointed[*i]).collect();
        // last resort for a malformed file: anything still untouched
        if roots.is_empty() {
            roots = (0..items.len()).filter(|i| parents[*i].is_none()).collect();
        }
    }

    let records: Vec<DirectoryRecord> = items
        .into_iter()
        .zip(parents)
        .map(|(dataset, parent)| DirectoryRecord { dataset, parent })
        .collect();
    (records, roots)
}

/// Positions of the record items in `bytes`, in sequence order (§3.2).
///
/// A scan for item headers (`FFFE,E000`) also finds the items nested inside a
/// record — an `IconImageSequence (0088,0200)` adds one per leaf — so the scan
/// is narrowed to the positions the stored links actually name. When that does
/// not identify every record (a dirty file with a broken chain), the trailing
/// run of headers is used as a best effort; when the bytes carry no usable
/// header at all (a dataset round-trip), item order is used.
fn record_offsets(bytes: &[u8], dataset: &Dataset, items: &[Dataset]) -> Vec<u32> {
    let mut scanned = Vec::new();
    for index in 0..bytes.len().saturating_sub(4) {
        if bytes[index] == 0xFE && bytes[index + 1] == 0xFF && bytes[index + 2] == 0x00 && bytes[index + 3] == 0xE0 {
            scanned.push(index as u32);
        }
    }

    let mut linked = HashSet::new();
    for item in items {
        for tag in [tags::OFFSET_NEXT, tags::OFFSET_LOWER] {
            if let Ok(offset) = item.get::<u32>(tag) {
                if offset != 0 {
                    linked.insert(offset);
                }
            }
        }
    }
    for tag in [tags::OFFSET_FIRST_ROOT, tags::OFFSET_LAST_ROOT] {
        if let Ok(offset) = dataset.get::<u32>(tag) {
            linked.insert(offset);
        }
    }

    let named: Vec<u32> = scanned.iter().copied().filter(|position| linked.contains(position)).collect();
    if named.len() == items.len() {
        return named;
    }
    if scanned.len() >= items.len() {
        return scanned[scanned.len() - items.len()..].to_vec();
    }
    (0..items.len()).map(|index| index as u32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CT: &str = "1.2.840.10008.5.1.4.1.1.2";

    fn instance(patient_id: &str, study: &str, series: &str, sop_class: &str, instance_uid: &str) -> DicomFile {
        let mut dataset = Dataset::new();
        dataset.insert_str(tags::PATIENT_ID, Vr::Lo, patient_id);
        dataset.insert_str(tags::PATIENT_NAME, Vr::Pn, "Doe^John");
        dataset.insert_str(tags::STUDY_INSTANCE_UID, Vr::Ui, study);
        dataset.insert_str(tags::SERIES_INSTANCE_UID, Vr::Ui, series);
        dataset.insert_str(tags::SOP_CLASS_UID, Vr::Ui, sop_class);
        dataset.insert_str(tags::SOP_INSTANCE_UID, Vr::Ui, instance_uid);
        dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        DicomFile::from_dataset(dataset)
    }

    fn temp_path(name: &str) -> PathBuf {
        // unique per (process, monotonic counter, name) so concurrent test
        // binaries or a recycled PID can never collide on the same file
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "radicom-dicomdir-{}-{seq}-{name}.dcm",
            std::process::id()
        ))
    }

    /// An 8x8 8-bit MONOCHROME2 image instance (enough for an icon).
    fn image_instance(patient_id: &str, study: &str, series: &str, instance_uid: &str) -> DicomFile {
        let mut dataset = Dataset::new();
        dataset.insert_str(tags::PATIENT_ID, Vr::Lo, patient_id);
        dataset.insert_str(tags::STUDY_INSTANCE_UID, Vr::Ui, study);
        dataset.insert_str(tags::SERIES_INSTANCE_UID, Vr::Ui, series);
        dataset.insert_str(tags::SOP_CLASS_UID, Vr::Ui, CT);
        dataset.insert_str(tags::SOP_INSTANCE_UID, Vr::Ui, instance_uid);
        dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        dataset.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
        dataset.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
        dataset.insert_value(Tag::ROWS, 8u16);
        dataset.insert_value(Tag::COLUMNS, 8u16);
        dataset.insert_value(Tag::BITS_ALLOCATED, 8u16);
        dataset.insert_value(Tag::BITS_STORED, 8u16);
        dataset.insert_value(Tag::HIGH_BIT, 7u16);
        dataset.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
        dataset.insert_str(Tag::NUMBER_OF_FRAMES, Vr::Cs, "1");
        dataset.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vec![128u8; 64])),
            ),
        ));
        DicomFile::from_dataset(dataset)
    }

    #[test]
    fn empty_file_set_id_is_rejected() {
        let result = std::panic::catch_unwind(|| DicomDirectory::new(""));
        assert!(result.is_err(), "an empty FileSetID must be rejected");
    }

    #[test]
    fn a_new_directory_names_itself_and_is_empty() {
        let dir = DicomDirectory::new("MYFS");
        assert_eq!(dir.file_set_id().as_deref(), Some("MYFS"));
        assert!(dir.records().is_empty());
        assert!(dir.roots().is_empty());
        assert_eq!(
            dir.file().file_meta.try_get_str(Tag::MEDIA_STORAGE_SOP_CLASS_UID).unwrap(),
            MEDIA_STORAGE_DIRECTORY_SOP_CLASS
        );
    }

    #[test]
    fn add_file_builds_the_patient_study_series_chain() {
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "IMG0001").unwrap();

        assert_eq!(dir.records().len(), 4, "PATIENT, STUDY, SERIES, IMAGE");
        assert_eq!(dir.roots().len(), 1);
        assert_eq!(dir.records()[0].record_type(), RecordType::Patient);
        assert_eq!(dir.records()[1].record_type(), RecordType::Study);
        assert_eq!(dir.records()[2].record_type(), RecordType::Series);
        assert_eq!(dir.records()[3].record_type(), RecordType::Image);

        let image = &dir.records()[3];
        assert_eq!(image.referenced_file_id(), Some(PathBuf::from("IMG0001")));
        assert_eq!(image.sop_instance_uid().as_deref(), Some("1.2.3.4.5"));
        assert_eq!(image.transfer_syntax_uid().as_deref(), Some("1.2.840.10008.1.2.1"));
        assert_eq!(image.parent, Some(2));

        assert_eq!(dir.children(0), vec![1]);
        assert_eq!(dir.children(1), vec![2]);
        assert_eq!(dir.children(2), vec![3]);
        assert!(dir.children(3).is_empty());
    }

    #[test]
    fn add_file_entry_returns_the_four_record_indices() {
        let mut dir = DicomDirectory::new("MYFS");
        let entry = dir
            .add_file_entry(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "IMG0001")
            .unwrap();

        assert_eq!((entry.patient, entry.study, entry.series, entry.instance), (0, 1, 2, 3));

        let reused = dir
            .add_file_entry(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.6"), "IMG0002")
            .unwrap();
        assert_eq!((reused.patient, reused.study, reused.series, reused.instance), (0, 1, 2, 4));
    }

    #[test]
    fn record_offsets_are_absolute_positions_of_item_tags() {
        // PS3.3 F.3.2.2: DICOMDIR record offsets are byte counts from the first
        // byte of the file (including the preamble, DICM prefix and file meta).
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "IMG0001").unwrap();
        let bytes = dir.to_bytes().unwrap();

        // absolute positions of every item tag (FFFE,E000) in the serialized file
        let item_positions: Vec<u32> = (0..bytes.len().saturating_sub(4))
            .filter(|i| bytes[*i] == 0xFE && bytes[*i + 1] == 0xFF && bytes[*i + 2] == 0x00 && bytes[*i + 3] == 0xE0)
            .map(|i| i as u32)
            .collect();
        assert_eq!(item_positions.len(), dir.records().len(), "one item tag per record");

        // the stored first-root offset equals the absolute position of the first item tag
        let first_root = dir.file().dataset.get::<u32>(tags::OFFSET_FIRST_ROOT).unwrap();
        assert_eq!(first_root, item_positions[0], "first-root offset is absolute from file start");

        // every next/child offset a record carries names a real item-tag position
        for record in dir.records() {
            for tag in [tags::OFFSET_NEXT, tags::OFFSET_LOWER] {
                if let Ok(offset) = record.dataset.get::<u32>(tag) {
                    if offset != 0 {
                        assert!(item_positions.contains(&offset), "offset {offset} points at an item tag");
                    }
                }
            }
        }
    }

    #[test]
    fn a_second_instance_shares_the_chain() {
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "IMG0001").unwrap();
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.6"), "IMG0002").unwrap();

        assert_eq!(dir.records().len(), 5);
        assert_eq!(dir.children(2).len(), 2, "both images hang under one series");
        assert_eq!(dir.records()[4].referenced_file_id(), Some(PathBuf::from("IMG0002")));
    }

    #[test]
    fn a_new_study_shares_the_patient() {
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "A").unwrap();
        dir.add_file(&instance("P1", "1.2.9", "1.2.9.4", CT, "1.2.9.4.5"), "B").unwrap();
        assert_eq!(dir.records().len(), 7);
        assert_eq!(dir.roots().len(), 1, "one patient");
        assert_eq!(dir.children(0).len(), 2, "two studies");
    }

    #[test]
    fn a_new_series_shares_the_study() {
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "A").unwrap();
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.9", CT, "1.2.3.9.5"), "B").unwrap();
        assert_eq!(dir.records().len(), 6);
        assert_eq!(dir.children(1).len(), 2, "one study, two series");
        assert_eq!(dir.children(0).len(), 1);
    }

    #[test]
    fn a_different_patient_is_a_second_root() {
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "A").unwrap();
        dir.add_file(&instance("P2", "1.2.9", "1.2.9.4", CT, "1.2.9.4.5"), "B").unwrap();
        assert_eq!(dir.roots().len(), 2);
        assert_eq!(dir.roots(), &[0, 4]);
    }

    #[test]
    fn storage_category_selects_the_leaf_record_type() {
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", "1.2.840.10008.5.1.4.1.1.88.33", "9.9.9"), "SR1")
            .unwrap();
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", "1.2.840.10008.5.1.4.1.1.11.2", "8.8.8"), "GS1")
            .unwrap();
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "7.7.7"), "CT1").unwrap();

        let kinds: Vec<RecordType> = dir.records().iter().filter(|r| r.parent == Some(2)).map(|r| r.record_type()).collect();
        assert_eq!(kinds, vec![RecordType::SrDocument, RecordType::Presentation, RecordType::Image]);
    }

    #[test]
    fn save_and_open_round_trip_the_tree() {
        let path = temp_path("roundtrip");
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "IMG0001").unwrap();
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.6"), "IMG0002").unwrap();
        dir.save(&path).unwrap();

        let reopened = DicomDirectory::open(&path).unwrap();
        assert_eq!(reopened.records().len(), 5, "every record survived");
        assert_eq!(reopened.roots(), &[0]);
        assert_eq!(reopened.records()[0].record_type(), RecordType::Patient);
        assert_eq!(reopened.children(2).len(), 2, "the sibling chain survived");
        assert_eq!(reopened.records()[4].referenced_file_id(), Some(PathBuf::from("IMG0002")));
        assert_eq!(reopened.file_set_id().as_deref(), Some("MYFS"));

        let dataset = &reopened.file().dataset;
        assert!(dataset.get::<u32>(tags::OFFSET_FIRST_ROOT).unwrap() > 0);
        assert!(dataset.get::<u32>(tags::OFFSET_LAST_ROOT).unwrap() > 0);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn written_offsets_are_real_byte_positions() {
        // offsets must point at record items, which is what a reader follows
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "A").unwrap();
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.6"), "B").unwrap();
        let bytes = dir.to_bytes().unwrap();

        // locate every item header (FFFE,E000) after the record sequence starts
        let mut item_positions = Vec::new();
        for index in 0..bytes.len().saturating_sub(4) {
            if bytes[index] == 0xFE && bytes[index + 1] == 0xFF && bytes[index + 2] == 0x00 && bytes[index + 3] == 0xE0 {
                item_positions.push(index as u32);
            }
        }
        assert_eq!(item_positions.len(), 5, "one item per record");

        // every stored offset must name a real item header position
        let reopened = DicomDirectory::from_bytes(&bytes).unwrap();
        let sequence = reopened.file().dataset.try_get_sequence(tags::DIR_RECORD_SEQUENCE).unwrap();
        for item in sequence.items() {
            if let Ok(lower) = item.get::<u32>(tags::OFFSET_LOWER) {
                if lower != 0 {
                    assert!(item_positions.contains(&lower), "lower offset {lower} is not an item position");
                }
            }
            if let Ok(next) = item.get::<u32>(tags::OFFSET_NEXT) {
                if next != 0 {
                    assert!(item_positions.contains(&next), "next offset {next} is not an item position");
                }
            }
        }

        // and the root offsets name the first and last root items
        let first_root = reopened.file().dataset.get::<u32>(tags::OFFSET_FIRST_ROOT).unwrap();
        assert_eq!(first_root, item_positions[0], "the first root offset names the first item");
    }

    #[test]
    fn add_file_with_icon_attaches_a_preview_that_survives_save() {
        use crate::media::icon::BasicIconGenerator;

        let path = temp_path("icon");
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file_with_icon(
            &image_instance("P1", "1.2.3", "1.2.3.4", "1.2.3.4.5"),
            "IMG0001",
            &BasicIconGenerator,
        )
        .unwrap();

        // the leaf record carries the icon sequence
        let leaf = &dir.records()[3];
        let icon_sequence = leaf.dataset.try_get_sequence(Tag::ICON_IMAGE_SEQUENCE).unwrap();
        assert_eq!(icon_sequence.items().len(), 1);
        let icon = &icon_sequence.items()[0];
        assert_eq!(icon.try_get_str(Tag::PHOTOMETRIC_INTERPRETATION).as_deref(), Some("MONOCHROME2"));

        // and the icon survives a save/open round-trip
        dir.save(&path).unwrap();
        let reopened = DicomDirectory::open(&path).unwrap();
        let leaf = &reopened.records()[3];
        let icon_sequence = leaf.dataset.try_get_sequence(Tag::ICON_IMAGE_SEQUENCE).unwrap();
        assert_eq!(icon_sequence.items().len(), 1, "the icon round-trips");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_failing_icon_generator_only_costs_the_preview() {
        // fo-dicom's AddIconSequence swallows icon errors: the record is added
        // without a preview rather than failing the whole directory
        struct Failing;
        impl IconGenerator for Failing {
            fn generate(&self, _dataset: &mut Dataset) -> Result<Option<Dataset>, Error> {
                Err(record_error("the icon could not be built"))
            }
        }

        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file_with_icon(&image_instance("P1", "1.2.3", "1.2.3.4", "1.2.3.4.5"), "IMG0001", &Failing)
            .expect("a failing icon generator must not abort the build");

        assert_eq!(dir.records().len(), 4, "the IMAGE record is still added");
        assert_eq!(dir.records()[3].record_type(), RecordType::Image);
        assert!(
            dir.records()[3].dataset.try_get_sequence(Tag::ICON_IMAGE_SEQUENCE).is_none(),
            "no icon sequence is attached"
        );

        // and the directory still writes and reopens as a valid four-record tree
        let bytes = dir.to_bytes().unwrap();
        let reopened = DicomDirectory::from_bytes(&bytes).unwrap();
        assert_eq!(reopened.walk(), vec![(0, 0), (1, 1), (2, 2), (3, 3)]);
    }

    /// Byte position of every record item whose `DirectoryRecordType` is
    /// `record_type`, in file order.
    ///
    /// Independent of the offsets under test: it finds the `(0004,1430)` CS
    /// element in the raw bytes and walks back to the item header that owns it.
    fn record_item_positions(bytes: &[u8], record_type: &str) -> Vec<u32> {
        let mut value = record_type.as_bytes().to_vec();
        if value.len() % 2 == 1 {
            value.push(b' '); // CS values are padded to even length
        }
        let mut needle = vec![0x04, 0x00, 0x30, 0x14, b'C', b'S'];
        needle.extend_from_slice(&(value.len() as u16).to_le_bytes());
        needle.extend_from_slice(&value);

        let mut positions = Vec::new();
        let mut from = 0;
        while let Some(offset) = bytes[from..].windows(needle.len()).position(|w| w == needle) {
            let element = from + offset;
            let header = (0..element)
                .rev()
                .find(|i| {
                    bytes[*i] == 0xFE && bytes[*i + 1] == 0xFF && bytes[*i + 2] == 0x00 && bytes[*i + 3] == 0xE0
                })
                .expect("the record type element is inside an item");
            positions.push(header as u32);
            from = element + needle.len();
        }
        positions
    }

    #[test]
    fn offsets_name_the_records_not_the_items_nested_in_them() {
        use crate::media::icon::BasicIconGenerator;

        // two 4-level chains; each leaf carries an IconImageSequence, so the
        // file holds more item headers than records
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file_with_icon(&image_instance("P1", "1.2.3", "1.2.3.4", "1.2.3.4.5"), "IMG0001", &BasicIconGenerator)
            .unwrap();
        dir.add_file_with_icon(&image_instance("P2", "9.8.7", "9.8.7.6", "9.8.7.6.5"), "IMG0002", &BasicIconGenerator)
            .unwrap();
        let bytes = dir.to_bytes().unwrap();

        let patients = record_item_positions(&bytes, "PATIENT");
        let studies = record_item_positions(&bytes, "STUDY");
        let series = record_item_positions(&bytes, "SERIES");
        let images = record_item_positions(&bytes, "IMAGE");
        assert_eq!((patients.len(), studies.len(), series.len(), images.len()), (2, 2, 2, 2));
        let item_headers = bytes
            .windows(4)
            .filter(|w| w[0] == 0xFE && w[1] == 0xFF && w[2] == 0x00 && w[3] == 0xE0)
            .count();
        assert_eq!(item_headers, 8 + 2, "the two icons add one item header each");

        // PS3.3 F.2.2: the root offsets name the first and last PATIENT record
        let dataset = &dir.file().dataset;
        assert_eq!(dataset.get::<u32>(tags::OFFSET_FIRST_ROOT).unwrap(), patients[0]);
        assert_eq!(dataset.get::<u32>(tags::OFFSET_LAST_ROOT).unwrap(), patients[1]);

        // and each level record points at its own child, not at a nested item
        let records = dir.records();
        assert_eq!(records[0].dataset.get::<u32>(tags::OFFSET_LOWER).unwrap(), studies[0]);
        assert_eq!(records[1].dataset.get::<u32>(tags::OFFSET_LOWER).unwrap(), series[0]);
        assert_eq!(records[2].dataset.get::<u32>(tags::OFFSET_LOWER).unwrap(), images[0]);
        assert_eq!(
            records[0].dataset.get::<u32>(tags::OFFSET_NEXT).unwrap(),
            patients[1],
            "the first patient links the second root"
        );
        assert!(records[3].dataset.get::<u32>(tags::OFFSET_LOWER).is_err(), "the leaf has no children");
        assert!(records[3].dataset.get::<u32>(tags::OFFSET_NEXT).is_err(), "the leaf ends the chain");

        // reopening maps the offsets back to the records: a full 8-record tree
        let reopened = DicomDirectory::from_bytes(&bytes).unwrap();
        assert_eq!(reopened.roots(), &[0, 4], "both PATIENT records are roots");
        assert_eq!(
            reopened.walk(),
            vec![(0, 0), (1, 1), (2, 2), (3, 3), (4, 0), (5, 1), (6, 2), (7, 3)],
            "each patient owns its own four-level chain"
        );
    }

    #[test]
    fn a_real_dicomdir_opens_with_the_whole_tree() {
        // tests-data/media/DICOMDIR: 20 patients, one STUDY/SERIES/IMAGE each,
        // written by another implementation, so the stored offsets are the only
        // thing tying the records together.
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests-data/media/DICOMDIR");
        let dir = DicomDirectory::open(&path).unwrap();

        assert_eq!(dir.records().len(), 80, "four levels for twenty patients");
        assert_eq!(dir.roots(), &(0..20).map(|i| i * 4).collect::<Vec<usize>>()[..], "one root per patient");
        assert_eq!(dir.walk().len(), 80, "every record is reachable from a root");
        for (index, depth) in dir.walk() {
            assert_eq!(depth, index % 4, "records are laid out patient to image");
            let expected = match index % 4 {
                0 => RecordType::Patient,
                1 => RecordType::Study,
                2 => RecordType::Series,
                _ => RecordType::Image,
            };
            assert_eq!(dir.records()[index].record_type(), expected, "record {index}");
        }
    }

    #[test]
    fn written_offsets_point_at_the_next_sibling_and_first_child() {
        // §4.2(b): asserting `offset != 0` proves a number was written, not that
        // it names the right record. Compare against item positions recovered
        // independently from the record-type values.
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "A").unwrap();
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.6"), "B").unwrap();
        let bytes = dir.to_bytes().unwrap();

        let studies = record_item_positions(&bytes, "STUDY");
        let images = record_item_positions(&bytes, "IMAGE");
        assert_eq!((studies.len(), images.len()), (1, 2), "one study, two images");

        let reopened = DicomDirectory::from_bytes(&bytes).unwrap();
        assert_eq!(
            reopened.records()[0].dataset.get::<u32>(tags::OFFSET_LOWER).unwrap(),
            studies[0],
            "the PATIENT points at its own STUDY record"
        );
        assert_eq!(
            reopened.records()[3].dataset.get::<u32>(tags::OFFSET_NEXT).unwrap(),
            images[1],
            "the first IMAGE links the second IMAGE record"
        );
        let second_image = reopened.records()[4].dataset.get::<u32>(tags::OFFSET_NEXT).ok();
        assert!(second_image.is_none() || second_image == Some(0), "the last image ends the chain");
    }

    #[test]
    fn lookups_find_records_by_key() {
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "A").unwrap();
        assert_eq!(dir.find_patient("P1"), Some(0));
        assert_eq!(dir.find_patient("P9"), None);
        assert_eq!(dir.find_study("1.2.3"), Some(1));
        assert_eq!(dir.find_series("1.2.3.4"), Some(2));
    }

    #[test]
    fn walk_visits_depth_first_with_depths() {
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "A").unwrap();
        let visited = dir.walk();
        assert_eq!(visited.len(), 4);
        assert_eq!(visited[0], (0, 0), "the patient is a root");
        assert_eq!(visited[3], (3, 3), "the image sits three levels down");
    }

    #[test]
    fn a_missing_identifying_key_is_rejected() {
        let mut dataset = Dataset::new();
        dataset.insert_str(tags::SOP_CLASS_UID, Vr::Ui, CT);
        dataset.insert_str(tags::SOP_INSTANCE_UID, Vr::Ui, "1.2.3");
        dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        let file = DicomFile::from_dataset(dataset);

        let mut dir = DicomDirectory::new("MYFS");
        let error = dir.add_file(&file, "X").unwrap_err();
        assert!(error.to_string().contains("Study Instance UID"), "got {error}");
        assert!(dir.records().is_empty(), "a failed add leaves no partial records");
    }

    #[test]
    fn file_ids_use_backslashes() {
        assert_eq!(path_to_file_id(Path::new("STUDY1/SERIES1/img.dcm")), "STUDY1\\SERIES1\\img.dcm");
        assert_eq!(path_to_file_id(Path::new("img.dcm")), "img.dcm");
    }

    #[test]
    fn record_types_round_trip_including_unknown_ones() {
        for kind in [
            RecordType::Patient,
            RecordType::Study,
            RecordType::Series,
            RecordType::Image,
            RecordType::SrDocument,
            RecordType::Presentation,
        ] {
            assert_eq!(RecordType::parse(kind.as_str()), kind);
        }
        let unknown = RecordType::parse("PRIVATE");
        assert_eq!(unknown.as_str(), "PRIVATE", "unknown types are kept verbatim");
    }

    #[test]
    fn a_record_renders_as_its_dataset_dump() {
        let mut dataset = Dataset::new();
        dataset.insert_str(tags::PATIENT_ID, Vr::Lo, "PAT-1");
        let record = DirectoryRecord { dataset, parent: None };
        let text = record.to_string();
        assert!(text.contains("PAT-1"), "Display renders the record dataset: {text}");
    }

    #[test]
    fn an_empty_directory_validates() {
        let dir = DicomDirectory::new("MYFS");
        dir.validate().unwrap();
    }

    #[test]
    fn a_built_directory_validates() {
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "A").unwrap();
        dir.validate().unwrap();
    }

    #[test]
    fn a_parent_cycle_is_reported_and_walk_terminates() {
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "A").unwrap();
        // records 2 (series) and 3 (image) link to each other, forming a cycle
        dir.records[2].parent = Some(3);
        dir.records[3].parent = Some(2);
        let error = dir.validate().unwrap_err();
        assert!(format!("{error:?}").contains("not reachable"), "a cycle leaves records unreachable: {error:?}");
        assert!(dir.walk().len() <= dir.records.len(), "walk must not recurse forever");
    }

    #[test]
    fn an_out_of_range_parent_is_reported() {
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "A").unwrap();
        dir.records[1].parent = Some(99);
        let error = dir.validate().unwrap_err();
        assert!(format!("{error:?}").contains("invalid parent"), "got {error:?}");
    }

    #[test]
    fn an_empty_directory_saves_without_root_offsets() {
        let mut dir = DicomDirectory::new("MYFS");
        let bytes = dir.to_bytes().unwrap();
        let reopened = DicomDirectory::from_bytes(&bytes).unwrap();
        assert!(reopened.records().is_empty());
        assert!(reopened.roots().is_empty());
        assert!(reopened.file().dataset.item(&tags::OFFSET_FIRST_ROOT).is_none());
    }

    #[test]
    fn a_corrupt_lower_offset_is_tolerated() {
        // a dirty DICOMDIR names an offset that does not exist (§3.2 tolerance)
        let mut dir = DicomDirectory::new("MYFS");
        dir.add_file(&instance("P1", "1.2.3", "1.2.3.4", CT, "1.2.3.4.5"), "A").unwrap();
        let mut bytes = dir.to_bytes().unwrap();

        // patch the first record's lower offset to an impossible value
        let mut file = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        {
            let mut items: Vec<Dataset> = file
                .dataset
                .try_get_sequence(tags::DIR_RECORD_SEQUENCE)
                .map(|s| s.items().to_vec())
                .unwrap();
            items[0].insert_value(tags::OFFSET_LOWER, 999_999u32);
            *file.dataset.insert_sequence(tags::DIR_RECORD_SEQUENCE).items_mut() = items;
        }
        bytes = file.to_bytes(&WriteOptions::default()).unwrap();

        let reopened = DicomDirectory::from_bytes(&bytes).unwrap();
        assert_eq!(reopened.records().len(), 4, "records still load");
        assert!(reopened.roots().contains(&0), "the patient becomes a root instead of failing");
    }

    #[test]
    fn malformed_bytes_are_a_typed_error() {
        assert!(DicomDirectory::from_bytes(&[0u8; 4]).is_err());
    }
}
