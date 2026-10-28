//! PS3.15 de-identification engine (ANON-01; docs/08-serialization-media.md §6.2).
//!
//! Applies a `SecurityProfile` to a dataset: walk every element, resolve the
//! action for its tag under the chosen option, and mutate. UIDs are rewritten
//! consistently (the same old UID always maps to the same new one) via a
//! per-run `replaced` map, and a file's media-storage SOP instance UID is kept
//! in sync with the dataset's SOP instance UID (§6.2).

use crate::error::Error;
use crate::io::file::DicomFile;
use crate::model::dataset::Dataset;
use crate::model::element::DicomElement;
use crate::model::item::DicomItem;
use crate::model::tag::Tag;
use crate::model::uid::Uid;
use crate::model::vr::Vr;
use crate::anonymizer::profile::{Action, AnonOption, SecurityProfile};
use std::collections::HashMap;

pub mod profile;

/// (0010,0010) PatientName.
const PATIENT_NAME: Tag = Tag::new(0x0010, 0x0010);
/// (0010,0020) PatientID.
const PATIENT_ID: Tag = Tag::new(0x0010, 0x0020);
/// (0008,0018) SOPInstanceUID.
const SOP_INSTANCE_UID: Tag = Tag::new(0x0008, 0x0018);
/// (0002,0003) MediaStorageSOPInstanceUID.
const MEDIA_STORAGE_SOP_INSTANCE_UID: Tag = Tag::new(0x0002, 0x0003);

/// Anonymizer that applies a profile under one option set (§6.2).
#[derive(Debug, Clone)]
pub struct Anonymizer {
    profile: SecurityProfile,
    option: AnonOption,
    patient_name: Option<String>,
    patient_id: Option<String>,
}

impl Anonymizer {
    /// An anonymizer over the built-in PS3.15 Basic Profile.
    pub fn basic() -> Self {
        Anonymizer {
            profile: SecurityProfile::basic(),
            option: AnonOption::BasicProfile,
            patient_name: None,
            patient_id: None,
        }
    }

    /// Uses a custom profile under a chosen option.
    pub fn with_profile(profile: SecurityProfile, option: AnonOption) -> Self {
        Anonymizer { profile, option, patient_name: None, patient_id: None }
    }

    /// Replaces PatientName / PatientID with the given values when set (§6.2).
    pub fn with_patient(mut self, name: impl Into<String>, id: impl Into<String>) -> Self {
        self.patient_name = Some(name.into());
        self.patient_id = Some(id.into());
        self
    }

    /// Anonymizes a dataset in place (§6.2).
    pub fn anonymize_in_place(&self, dataset: &mut Dataset) -> Result<(), Error> {
        let mut replaced = HashMap::new();
        self.walk(dataset, &mut replaced)
    }

    /// Anonymizes a file in place, syncing the media-storage SOP instance UID.
    pub fn anonymize_file_in_place(&self, file: &mut DicomFile) -> Result<(), Error> {
        self.anonymize_in_place(&mut file.dataset)?;
        // §6.2: keep the file-meta instance UID equal to the (now anonymized)
        // dataset SOP instance UID so the two never disagree
        if file.file_meta.contains(&MEDIA_STORAGE_SOP_INSTANCE_UID) {
            if let Some(sop_instance) = file.dataset.try_get_str(SOP_INSTANCE_UID) {
                file.file_meta.insert_str(MEDIA_STORAGE_SOP_INSTANCE_UID, Vr::Ui, sop_instance);
            }
        }
        Ok(())
    }

    /// Clones and anonymizes a dataset (§6.2).
    pub fn anonymize(&self, dataset: &Dataset) -> Result<Dataset, Error> {
        let mut clone = dataset.clone();
        self.anonymize_in_place(&mut clone)?;
        Ok(clone)
    }

    /// Clones and anonymizes a file (§6.2).
    pub fn anonymize_file(&self, file: &DicomFile) -> Result<DicomFile, Error> {
        let mut clone = file.clone();
        self.anonymize_file_in_place(&mut clone)?;
        Ok(clone)
    }

    /// The recursive walk: apply each element's action, recurse into sequences
    /// unless the action is K (§6.2).
    fn walk(&self, dataset: &mut Dataset, replaced: &mut HashMap<String, String>) -> Result<(), Error> {
        let mut to_remove: Vec<Tag> = Vec::new();
        let items: Vec<DicomItem> = dataset.items().cloned().collect();

        for item in &items {
            let tag = item.tag().clone();
            let tag_text = format!("{:04X},{:04X}", tag.group(), tag.element());
            let action = self.profile.action_for(&tag_text, self.option);

            // sequences: a Remove action deletes the whole sequence; otherwise
            // recurse into the children (Keep leaves the sequence and its
            // children untouched). Without the Remove case an SQ tag marked X
            // would survive as an empty/kept shell.
            if let DicomItem::Sequence(sequence) = item {
                match action {
                    Some(Action::Remove) => to_remove.push(tag.clone()),
                    Some(Action::Keep) => {}
                    _ => {
                        let mut new_items: Vec<Dataset> = sequence.items().to_vec();
                        for child in &mut new_items {
                            self.walk(child, replaced)?;
                        }
                        *dataset
                            .insert_sequence(tag.clone())
                            .items_mut() = new_items;
                    }
                }
                continue;
            }

            let Some(action) = action else { continue };
            match action {
                Action::Keep => {}
                Action::Remove => to_remove.push(tag.clone()),
                Action::ZeroLength => self.blank(dataset, item, false)?,
                Action::Dummy => {
                    // UID → new UID; numeric strings (DS/IS) → "0"; other typed
                    // values via blank() (dates → MinValue, binary → zero dummy,
                    // generic strings → "ANONYMOUS") so D never writes an
                    // invalid value such as "ANONYMOUS" into a DS/IS element.
                    if item.vr() == Vr::Ui {
                        self.replace_uid(dataset, item, replaced)?;
                    } else {
                        self.blank(dataset, item, true)?;
                    }
                }
                Action::Clean | Action::Uid => {
                    if item.vr() == Vr::Ui {
                        self.replace_uid(dataset, item, replaced)?;
                    } else if item.vr().is_string() {
                        dataset.insert_str(tag.clone(), item.vr(), "ANONYMOUS");
                    } else {
                        self.blank(dataset, item, true)?;
                    }
                }
            }

            // per-tag patient overrides (§6.2)
            if tag == PATIENT_NAME {
                if let Some(name) = &self.patient_name {
                    dataset.insert_str(tag.clone(), Vr::Pn, name.clone());
                }
            } else if tag == PATIENT_ID {
                if let Some(id) = &self.patient_id {
                    dataset.insert_str(tag.clone(), Vr::Lo, id.clone());
                }
            }
        }

        for tag in to_remove {
            dataset.remove(&tag);
        }
        Ok(())
    }

    /// Rewrites a UID element to a consistent replacement (§6.2).
    fn replace_uid(
        &self,
        dataset: &mut Dataset,
        item: &DicomItem,
        replaced: &mut HashMap<String, String>,
    ) -> Result<(), Error> {
        let DicomItem::Element(element) = item else { return Ok(()) };
        let old = element.string_value().to_string();
        if old.trim().is_empty() {
            return Ok(()); // no need to replace empty UIDs
        }
        let new_uid = replaced
            .entry(old.clone())
            .or_insert_with(Uid::generate_value)
            .clone();
        dataset.insert_str(element.tag().clone(), Vr::Ui, new_uid);
        Ok(())
    }

    /// Blanks an element to a value suitable for its VR (§6.2, fo-dicom
    /// `BlankItem`). `non_zero` requests a dummy (D) rather than zero-length (Z).
    fn blank(&self, dataset: &mut Dataset, item: &DicomItem, non_zero: bool) -> Result<(), Error> {
        let DicomItem::Element(element) = item else { return Ok(()) };
        let tag = element.tag().clone();
        let vr = element.vr();

        // date/time: fo-dicom writes an empty (Z) or MinValue (D) value
        if matches!(vr, Vr::Da | Vr::Dt | Vr::Tm) {
            if non_zero {
                let dummy = match vr {
                    Vr::Da => "19000101",
                    Vr::Dt => "19000101000000",
                    _ => "000000",
                };
                dataset.insert_str(tag, vr, dummy);
            } else {
                dataset.insert_empty(tag, vr);
            }
            return Ok(());
        }

        if vr.is_string() {
            if !non_zero {
                dataset.insert_str(tag, vr, "");
            } else if matches!(vr, Vr::Ds | Vr::Is) {
                // numeric-string VRs need a legal decimal/integer dummy, not words
                dataset.insert_str(tag, vr, "0");
            } else {
                dataset.insert_str(tag, vr, "ANONYMOUS");
            }
            return Ok(());
        }

        // binary / numeric: empty bytes for Z; a single zero value for D
        if non_zero && !is_other(vr) && vr.unit_size() > 1 {
            let bytes = vec![0u8; vr.unit_size()];
            dataset.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
                tag,
                vr,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(bytes)),
            )));
        } else {
            dataset.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
                tag,
                vr,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(Vec::new())),
            )));
        }
        Ok(())
    }
}

/// Binary VRs (`Other*`/UN) that blank to an empty buffer rather than a dummy.
fn is_other(vr: Vr) -> bool {
    matches!(vr, Vr::Ob | Vr::Od | Vr::Of | Vr::Ol | Vr::Ov | Vr::Ow | Vr::Un)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::dataset::Dataset;
    use crate::model::vr::Vr;

    fn dataset() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Doe^John");
        ds.insert_str(Tag::new(0x0010, 0x0020), Vr::Lo, "12345");
        ds.insert_str(Tag::new(0x0008, 0x0050), Vr::Sh, "ACC-1"); // AccessionNumber: Z
        ds.insert_str(Tag::new(0x0008, 0x0018), Vr::Ui, "1.2.3.4"); // SOPInstanceUID: U
        ds.insert_str(Tag::new(0x0008, 0x0060), Vr::Cs, "CT"); // Modality: not in table → keep
        ds
    }

    #[test]
    fn patient_name_and_id_are_blanked_under_the_basic_profile() {
        // fo-dicom's basic profile marks both Z, so they blank (not "ANONYMOUS")
        let mut ds = dataset();
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        assert_eq!(ds.try_get_str(Tag::new(0x0010, 0x0010)).as_deref(), Some(""));
        assert_eq!(ds.try_get_str(Tag::new(0x0010, 0x0020)).as_deref(), Some(""));
    }

    #[test]
    fn an_owned_uid_is_replaced_and_consistent() {
        let mut ds = dataset();
        ds.insert_str(Tag::new(0x0020, 0x000D), Vr::Ui, "1.2.3.4"); // StudyInstanceUID: U
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        let sop = ds.try_get_str(Tag::new(0x0008, 0x0018)).unwrap();
        let study = ds.try_get_str(Tag::new(0x0020, 0x000D)).unwrap();
        assert_ne!(sop, "1.2.3.4", "the SOP instance UID must be replaced");
        assert_eq!(sop, study, "the same source UID maps to the same replacement");
        assert!(!sop.is_empty());
    }

    #[test]
    fn a_keep_tag_is_untouched() {
        let mut ds = dataset();
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        assert_eq!(ds.try_get_str(Tag::new(0x0008, 0x0060)).as_deref(), Some("CT"));
    }

    #[test]
    fn a_zero_length_tag_is_blanked() {
        let mut ds = dataset();
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        let accession = ds.try_get_str(Tag::new(0x0008, 0x0050));
        assert!(accession.map(|v| v.is_empty()).unwrap_or(true), "AccessionNumber is blanked");
    }

    #[test]
    fn patient_overrides_apply() {
        let mut ds = dataset();
        Anonymizer::basic()
            .with_patient("Alias^Name", "PAT-42")
            .anonymize_in_place(&mut ds)
            .unwrap();
        assert_eq!(ds.try_get_str(Tag::new(0x0010, 0x0010)).as_deref(), Some("Alias^Name"));
        assert_eq!(ds.try_get_str(Tag::new(0x0010, 0x0020)).as_deref(), Some("PAT-42"));
    }

    #[test]
    fn a_ds_is_dummied_with_a_numeric_value_not_anonymous() {
        // SpacingBetweenSlices (0018,0088) is DS. D must produce a legal decimal
        // string ("0"), never "ANONYMOUS" which would be an invalid DS value.
        let csv = "0018,0088;D\n";
        let profile = SecurityProfile::parse(csv).unwrap();
        let anon = Anonymizer::with_profile(profile, AnonOption::BasicProfile);
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0018, 0x0088), Vr::Ds, "12.5");
        anon.anonymize_in_place(&mut ds).unwrap();
        let v = ds.try_get_str(Tag::new(0x0018, 0x0088)).unwrap_or_default();
        assert_ne!(v, "ANONYMOUS", "a DS dummy must be numeric, not the word ANONYMOUS");
        assert!(v.trim().parse::<f64>().is_ok(), "DS dummy must parse as a number, got {v:?}");
    }

    #[test]
    fn a_sequence_marked_remove_under_the_basic_profile_is_deleted() {
        // (0008,1110) ReferencedStudySequence is X/Z under the basic profile, so
        // the basic option removes the whole sequence (not just its children).
        let mut ds = Dataset::new();
        let mut child = Dataset::new();
        child.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Nested^Name");
        ds.insert_sequence(Tag::new(0x0008, 0x1110)).push(child);

        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        assert!(
            ds.try_get_sequence(Tag::new(0x0008, 0x1110)).is_none(),
            "ReferencedStudySequence is X under the basic profile and must be removed"
        );
    }

    #[test]
    fn sequence_children_are_anonymized_recursively() {
        // A sequence the profile keeps-but-processes (D/C/U/Z) is recursed into;
        // its children are anonymized. (K would leave the whole subtree untouched.)
        let csv = "0008,1110;D\n0010,0010;Z\n";
        let profile = SecurityProfile::parse(csv).unwrap();
        let anon = Anonymizer::with_profile(profile, AnonOption::BasicProfile);

        let mut ds = Dataset::new();
        let mut child = Dataset::new();
        child.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Nested^Name");
        ds.insert_sequence(Tag::new(0x0008, 0x1110)).push(child);

        anon.anonymize_in_place(&mut ds).unwrap();
        let sequence = ds.try_get_sequence(Tag::new(0x0008, 0x1110)).unwrap();
        assert_eq!(
            sequence.items()[0].try_get_str(Tag::new(0x0010, 0x0010)).as_deref(),
            Some(""),
            "the nested PatientName is blanked"
        );
    }

    #[test]
    fn the_clone_variant_leaves_the_original_untouched() {
        let ds = dataset();
        let anonymized = Anonymizer::basic().anonymize(&ds).unwrap();
        assert_eq!(ds.try_get_str(Tag::new(0x0010, 0x0010)).as_deref(), Some("Doe^John"));
        assert_eq!(anonymized.try_get_str(Tag::new(0x0010, 0x0010)).as_deref(), Some(""));
    }

    #[test]
    fn a_date_tag_marked_z_is_emptied_and_one_marked_x_is_removed() {
        // cross-checked against fo-dicom's DicomAnonymizerTest:
        // StudyDate (0008,0020)=Z → empty; SeriesDate (0008,0021)=X/D → removed
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0008, 0x0020), Vr::Da, "20260101"); // StudyDate: Z
        ds.insert_str(Tag::new(0x0008, 0x0021), Vr::Da, "20260102"); // SeriesDate: X/D
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();

        assert_eq!(ds.try_get_str(Tag::new(0x0008, 0x0020)).as_deref(), Some(""), "StudyDate blanked");
        assert!(ds.try_get_str(Tag::new(0x0008, 0x0021)).is_none(), "SeriesDate removed");
    }

    #[test]
    fn two_identical_source_uids_map_to_one_replacement() {
        // fo-dicom keeps a per-run map so consistency holds even across items
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0008, 0x0018), Vr::Ui, "1.2.3.4"); // SOPInstanceUID
        ds.insert_str(Tag::new(0x0008, 0x1155), Vr::Ui, "1.2.3.4"); // ReferencedSOPInstanceUID
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        let a = ds.try_get_str(Tag::new(0x0008, 0x0018)).unwrap();
        let b = ds.try_get_str(Tag::new(0x0008, 0x1155)).unwrap();
        assert_eq!(a, b, "the same UID must map consistently even across different tags");
        assert_ne!(a, "1.2.3.4");
    }

    #[test]
    fn a_sequence_marked_remove_is_deleted_whole() {
        // custom profile: ReferencedStudySequence (0008,1110) = X under the
        // basic option. The whole sequence must be removed, not kept as a shell.
        let csv = "0008,1110;X\n";
        let profile = SecurityProfile::parse(csv).unwrap();
        let anon = Anonymizer::with_profile(profile, AnonOption::BasicProfile);

        let mut ds = Dataset::new();
        let mut child = Dataset::new();
        child.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Nested^Name");
        ds.insert_sequence(Tag::new(0x0008, 0x1110)).push(child);

        anon.anonymize_in_place(&mut ds).unwrap();
        assert!(
            ds.try_get_sequence(Tag::new(0x0008, 0x1110)).is_none(),
            "an SQ marked X must be removed entirely"
        );
    }

    #[test]
    fn a_sequence_marked_dummy_still_recurses_into_children() {
        // Z/D/C/U on an SQ keep the sequence but process its children.
        let csv = "0008,1110;D\n0010,0020;Z\n";
        let profile = SecurityProfile::parse(csv).unwrap();
        let anon = Anonymizer::with_profile(profile, AnonOption::BasicProfile);

        let mut ds = Dataset::new();
        let mut child = Dataset::new();
        child.insert_str(Tag::new(0x0010, 0x0020), Vr::Lo, "PII"); // PatientID: Z under basic
        ds.insert_sequence(Tag::new(0x0008, 0x1110)).push(child);

        anon.anonymize_in_place(&mut ds).unwrap();
        let seq = ds.try_get_sequence(Tag::new(0x0008, 0x1110)).expect("sequence is kept");
        assert_eq!(
            seq.items()[0].try_get_str(Tag::new(0x0010, 0x0020)).as_deref(),
            Some(""),
            "the child PatientID is still anonymized"
        );
    }

    #[test]
    fn anonymizing_a_file_syncs_the_media_storage_uid() {
        let mut file = DicomFile::from_dataset(dataset());
        file.file_meta.insert_str(Tag::new(0x0002, 0x0003), Vr::Ui, "1.2.3.4");
        Anonymizer::basic().anonymize_file_in_place(&mut file).unwrap();
        let sop = file.dataset.try_get_str(Tag::new(0x0008, 0x0018)).unwrap();
        let meta = file.file_meta.try_get_str(Tag::new(0x0002, 0x0003)).unwrap();
        assert_eq!(meta, sop, "file meta must track the anonymized instance UID");
    }
}
