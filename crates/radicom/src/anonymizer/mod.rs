//! PS3.15 de-identification engine (ANON-01; docs/01-architecture.md §12).
//!
//! Applies a `SecurityProfile` to a dataset: walk every element, resolve the
//! action for its tag under the chosen option, and mutate. UIDs are rewritten
//! consistently through a map that lives on the `Anonymizer` (fo-dicom's
//! `ReplacedUIDs`), so the same old UID maps to the same new one across every
//! dataset and file anonymized by that instance; a file's media-storage SOP
//! instance UID is kept in sync with the dataset's SOP instance UID (§12.5).

use crate::anonymizer::profile::{Action, AnonOption, SecurityProfile};
use crate::error::Error;
use crate::io::file::DicomFile;
use crate::model::dataset::Dataset;
use crate::model::element::DicomElement;
use crate::model::item::DicomItem;
use crate::model::tag::Tag;
use crate::model::uid::Uid;
use crate::model::vr::Vr;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub mod profile;

/// (0010,0010) PatientName.
const PATIENT_NAME: Tag = Tag::PATIENT_NAME;
/// (0010,0020) PatientID.
const PATIENT_ID: Tag = Tag::PATIENT_ID;
/// (0008,0018) SOPInstanceUID.
const SOP_INSTANCE_UID: Tag = Tag::SOP_INSTANCE_UID;
/// (0002,0003) MediaStorageSOPInstanceUID.
const MEDIA_STORAGE_SOP_INSTANCE_UID: Tag = Tag::MEDIA_STORAGE_SOP_INSTANCE_UID;

/// Anonymizer that applies a profile under one option set (§12.5).
#[derive(Debug, Clone)]
pub struct Anonymizer {
    profile: SecurityProfile,
    option: AnonOption,
    patient_name: Option<String>,
    patient_id: Option<String>,
    /// Original → replacement UID, shared by clones and kept across calls
    /// (fo-dicom `ReplacedUIDs`); a `Mutex` so the methods can stay `&self`.
    replaced: Arc<Mutex<HashMap<String, String>>>,
}

impl Anonymizer {
    /// An anonymizer over the built-in PS3.15 Basic Profile.
    pub fn basic() -> Self {
        Anonymizer {
            profile: SecurityProfile::basic(),
            option: AnonOption::BasicProfile,
            patient_name: None,
            patient_id: None,
            replaced: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Uses a custom profile under a chosen option.
    pub fn with_profile(profile: SecurityProfile, option: AnonOption) -> Self {
        Anonymizer {
            profile,
            option,
            patient_name: None,
            patient_id: None,
            replaced: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Replaces PatientName / PatientID with the given values when set (§12.5).
    pub fn with_patient(mut self, name: impl Into<String>, id: impl Into<String>) -> Self {
        self.patient_name = Some(name.into());
        self.patient_id = Some(id.into());
        self
    }

    /// The original → replacement UID map accumulated so far (§12.5, fo-dicom
    /// `ReplacedUIDs`). Two instances never share it.
    pub fn replaced_uids(&self) -> HashMap<String, String> {
        lock(&self.replaced).clone()
    }

    /// Anonymizes a dataset in place (§12.5).
    pub fn anonymize_in_place(&self, dataset: &mut Dataset) -> Result<(), Error> {
        let mut replaced = lock(&self.replaced);
        self.walk(dataset, &mut replaced)
    }

    /// Anonymizes a file in place, syncing the media-storage SOP instance UID.
    pub fn anonymize_file_in_place(&self, file: &mut DicomFile) -> Result<(), Error> {
        self.anonymize_in_place(&mut file.dataset)?;
        // §12.5: keep the file-meta instance UID equal to the (now anonymized)
        // dataset SOP instance UID so the two never disagree
        if file.file_meta.contains(&MEDIA_STORAGE_SOP_INSTANCE_UID) {
            if let Some(sop_instance) = file.dataset.try_get_str(SOP_INSTANCE_UID) {
                file.file_meta
                    .insert_str(MEDIA_STORAGE_SOP_INSTANCE_UID, Vr::Ui, sop_instance);
            }
        }
        Ok(())
    }

    /// Clones and anonymizes a dataset (§12.5).
    pub fn anonymize(&self, dataset: &Dataset) -> Result<Dataset, Error> {
        let mut clone = dataset.clone();
        self.anonymize_in_place(&mut clone)?;
        Ok(clone)
    }

    /// Clones and anonymizes a file (§12.5).
    pub fn anonymize_file(&self, file: &DicomFile) -> Result<DicomFile, Error> {
        let mut clone = file.clone();
        self.anonymize_file_in_place(&mut clone)?;
        Ok(clone)
    }

    /// The recursive walk: apply each element's action, recurse into sequences
    /// unless the action is K (§12.5).
    ///
    /// Only the tags are collected up front — the items themselves stay in the
    /// dataset, so a 32 MB pixel buffer or a whole SR subtree is never cloned
    /// just to be walked.
    fn walk(
        &self,
        dataset: &mut Dataset,
        replaced: &mut HashMap<String, String>,
    ) -> Result<(), Error> {
        let mut to_remove: Vec<Tag> = Vec::new();
        let tags: Vec<Tag> = dataset.items().map(|item| item.tag().clone()).collect();

        for tag in tags {
            let action = self.profile.action_for_tag(&tag, self.option);

            // sequences, exactly like fo-dicom `AnonymizeInPlace`: recurse only
            // for a null or K action (K means "cleaned for sequences"); D/Z/U/C
            // replace the whole sequence with an empty one (`BlankItem`), and X
            // removes it. Recursing a D sequence (e.g. ContentSequence) would
            // keep the children the profile means to delete — an SR free-text
            // PHI leak. The sequence is moved out and back rather than cloned,
            // so only the traversal is paid for.
            if matches!(dataset.item(&tag), Some(DicomItem::Sequence(_))) {
                match action {
                    None | Some(Action::Keep) => {
                        let Some(DicomItem::Sequence(mut sequence)) = dataset.remove(&tag) else {
                            continue;
                        };
                        for child in sequence.items_mut() {
                            self.walk(child, replaced)?;
                        }
                        dataset.insert_or_update(DicomItem::Sequence(sequence));
                    }
                    Some(Action::Remove) => to_remove.push(tag),
                    // D/Z/U/C on a sequence: an empty sequence
                    Some(_) => {
                        dataset.insert_sequence(tag);
                    }
                }
                continue;
            }

            if let Some(action) = action {
                if action == Action::Remove {
                    to_remove.push(tag.clone());
                    self.apply_patient_override(dataset, &tag);
                    continue;
                }
                // fragments carry no VR and no value the actions know how to blank
                let Some(DicomItem::Element(element)) = dataset.item(&tag) else {
                    self.apply_patient_override(dataset, &tag);
                    continue;
                };
                let vr = element.vr();

                match action {
                    Action::Keep => {}
                    Action::Remove => unreachable!("handled above"),
                    Action::ZeroLength => self.blank(dataset, &tag, vr, false)?,
                    // UID → new UID; numeric strings (DS/IS) → "0"; other typed
                    // values via blank() (dates → MinValue, binary → zero dummy,
                    // generic strings → "ANONYMOUS") so D never writes an invalid
                    // value such as "ANONYMOUS" into a DS/IS element.
                    Action::Dummy if vr == Vr::Ui => self.replace_uid(dataset, &tag, replaced),
                    Action::Dummy => self.blank(dataset, &tag, vr, true)?,
                    Action::Clean | Action::Uid if vr == Vr::Ui => {
                        self.replace_uid(dataset, &tag, replaced)
                    }
                    Action::Clean | Action::Uid => {
                        if vr.is_string() {
                            dataset.insert_str(tag.clone(), vr, "ANONYMOUS");
                        } else {
                            self.blank(dataset, &tag, vr, true)?;
                        }
                    }
                }
            }

            // Per-tag patient overrides (§12.5): fo-dicom applies them to every
            // item, whether or not the profile matched the tag. Removing the tag
            // still wins because `to_remove` is applied after the walk.
            self.apply_patient_override(dataset, &tag);
        }

        for tag in to_remove {
            dataset.remove(&tag);
        }
        Ok(())
    }

    /// Applies the per-tag patient overrides (§12.5).
    ///
    /// fo-dicom runs these checks outside the profile-action switch, so a custom
    /// profile that does not list PatientName/PatientID still gets the requested
    /// replacement values.
    fn apply_patient_override(&self, dataset: &mut Dataset, tag: &Tag) {
        if *tag == PATIENT_NAME {
            if let Some(name) = &self.patient_name {
                dataset.insert_str(tag.clone(), Vr::Pn, name.clone());
            }
        } else if *tag == PATIENT_ID {
            if let Some(id) = &self.patient_id {
                dataset.insert_str(tag.clone(), Vr::Lo, id.clone());
            }
        }
    }

    /// Rewrites a UID element to a consistent replacement (§12.5).
    fn replace_uid(
        &self,
        dataset: &mut Dataset,
        tag: &Tag,
        replaced: &mut HashMap<String, String>,
    ) {
        let old = dataset.try_get_str(tag).unwrap_or_default();
        if old.trim().is_empty() {
            return; // no need to replace empty UIDs
        }
        let new_uid = replaced
            .entry(old)
            .or_insert_with(Uid::generate_value)
            .clone();
        dataset.insert_str(tag.clone(), Vr::Ui, new_uid);
    }

    /// Blanks an element to a value suitable for its VR (§12.5, fo-dicom
    /// `BlankItem`). `non_zero` requests a dummy (D) rather than zero-length (Z).
    fn blank(&self, dataset: &mut Dataset, tag: &Tag, vr: Vr, non_zero: bool) -> Result<(), Error> {
        let tag = tag.clone();

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
    matches!(
        vr,
        Vr::Ob | Vr::Od | Vr::Of | Vr::Ol | Vr::Ov | Vr::Ow | Vr::Un
    )
}

/// Locks the UID map; a poisoned mutex still holds a consistent map.
fn lock(
    map: &Mutex<HashMap<String, String>>,
) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
    map.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::dataset::Dataset;
    use crate::model::vr::Vr;

    fn dataset() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "12345");
        ds.insert_str(Tag::ACCESSION_NUMBER, Vr::Sh, "ACC-1"); // AccessionNumber: Z
        ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4"); // SOPInstanceUID: U
        ds.insert_str(Tag::MODALITY, Vr::Cs, "CT"); // Modality: not in table → keep
        ds
    }

    #[test]
    fn patient_name_and_id_are_blanked_under_the_basic_profile() {
        // fo-dicom's basic profile marks both Z, so they blank (not "ANONYMOUS")
        let mut ds = dataset();
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        assert_eq!(ds.try_get_str(Tag::PATIENT_NAME).as_deref(), Some(""));
        assert_eq!(ds.try_get_str(Tag::PATIENT_ID).as_deref(), Some(""));
    }

    #[test]
    fn an_owned_uid_is_replaced_and_consistent() {
        let mut ds = dataset();
        ds.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, "1.2.3.4"); // StudyInstanceUID: U
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        let sop = ds.try_get_str(Tag::SOP_INSTANCE_UID).unwrap();
        let study = ds.try_get_str(Tag::STUDY_INSTANCE_UID).unwrap();
        assert_ne!(sop, "1.2.3.4", "the SOP instance UID must be replaced");
        assert_eq!(
            sop, study,
            "the same source UID maps to the same replacement"
        );
        assert!(!sop.is_empty());
    }

    #[test]
    fn a_keep_tag_is_untouched() {
        let mut ds = dataset();
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        assert_eq!(ds.try_get_str(Tag::MODALITY).as_deref(), Some("CT"));
    }

    #[test]
    fn a_zero_length_tag_is_blanked() {
        let mut ds = dataset();
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        let accession = ds.try_get_str(Tag::ACCESSION_NUMBER);
        assert!(
            accession.map(|v| v.is_empty()).unwrap_or(true),
            "AccessionNumber is blanked"
        );
    }

    #[test]
    fn patient_overrides_apply() {
        let mut ds = dataset();
        Anonymizer::basic()
            .with_patient("Alias^Name", "PAT-42")
            .anonymize_in_place(&mut ds)
            .unwrap();
        assert_eq!(
            ds.try_get_str(Tag::PATIENT_NAME).as_deref(),
            Some("Alias^Name")
        );
        assert_eq!(ds.try_get_str(Tag::PATIENT_ID).as_deref(), Some("PAT-42"));
    }

    #[test]
    fn a_ds_is_dummied_with_a_numeric_value_not_anonymous() {
        // SpacingBetweenSlices (0018,0088) is DS. D must produce a legal decimal
        // string ("0"), never "ANONYMOUS" which would be an invalid DS value.
        let csv = "0018,0088;D\n";
        let profile = SecurityProfile::parse(csv).unwrap();
        let anon = Anonymizer::with_profile(profile, AnonOption::BasicProfile);
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SPACING_BETWEEN_SLICES, Vr::Ds, "12.5");
        anon.anonymize_in_place(&mut ds).unwrap();
        let v = ds
            .try_get_str(Tag::SPACING_BETWEEN_SLICES)
            .unwrap_or_default();
        assert_ne!(
            v, "ANONYMOUS",
            "a DS dummy must be numeric, not the word ANONYMOUS"
        );
        assert!(
            v.trim().parse::<f64>().is_ok(),
            "DS dummy must parse as a number, got {v:?}"
        );
    }

    #[test]
    fn a_sequence_marked_remove_under_the_basic_profile_is_deleted() {
        // (0008,1110) ReferencedStudySequence is X/Z under the basic profile, so
        // the basic option removes the whole sequence (not just its children).
        let mut ds = Dataset::new();
        let mut child = Dataset::new();
        child.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Nested^Name");
        ds.insert_sequence(Tag::REFERENCED_STUDY_SEQUENCE)
            .push(child);

        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        assert!(
            ds.try_get_sequence(Tag::REFERENCED_STUDY_SEQUENCE)
                .is_none(),
            "ReferencedStudySequence is X under the basic profile and must be removed"
        );
    }

    #[test]
    fn a_keep_sequence_is_recursed_so_its_children_are_cleaned() {
        // fo-dicom recurses a sequence whose action is K ("cleaned for
        // sequences"): the sequence survives, its children are anonymized.
        let csv = "0008,1110;K\n0010,0010;Z\n";
        let profile = SecurityProfile::parse(csv).unwrap();
        let anon = Anonymizer::with_profile(profile, AnonOption::BasicProfile);

        let mut ds = Dataset::new();
        let mut child = Dataset::new();
        child.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Nested^Name");
        ds.insert_sequence(Tag::REFERENCED_STUDY_SEQUENCE)
            .push(child);

        anon.anonymize_in_place(&mut ds).unwrap();
        let sequence = ds.try_get_sequence(Tag::REFERENCED_STUDY_SEQUENCE).unwrap();
        assert_eq!(
            sequence.items()[0]
                .try_get_str(Tag::PATIENT_NAME)
                .as_deref(),
            Some(""),
            "the nested PatientName is blanked even under K"
        );
    }

    #[test]
    fn a_sequence_marked_dummy_or_zero_is_emptied_not_recursed() {
        // fo-dicom BlankItem replaces a D/Z/U/C sequence with an empty one; it
        // must not keep the children (that is the SR content PHI leak).
        for action in ["D", "Z", "U", "C"] {
            let csv = format!("0008,1110;{action}\n");
            let profile = SecurityProfile::parse(&csv).unwrap();
            let anon = Anonymizer::with_profile(profile, AnonOption::BasicProfile);

            let mut ds = Dataset::new();
            let mut child = Dataset::new();
            child.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Nested^Name");
            ds.insert_sequence(Tag::REFERENCED_STUDY_SEQUENCE)
                .push(child);

            anon.anonymize_in_place(&mut ds).unwrap();
            assert!(
                ds.try_get_sequence(Tag::REFERENCED_STUDY_SEQUENCE)
                    .is_some(),
                "{action} keeps the sequence"
            );
            assert!(
                ds.try_get_sequence(Tag::REFERENCED_STUDY_SEQUENCE)
                    .unwrap()
                    .items()
                    .is_empty(),
                "a sequence marked {action} must be emptied, not recursed"
            );
        }
    }

    #[test]
    fn sr_content_sequence_free_text_is_gone_after_basic_anonymization() {
        // Regression (FEAT-01): the Basic profile marks ContentSequence D. The
        // whole sequence must be emptied, or the SR free text (which has no
        // profile row of its own) leaks.
        let mut ds = Dataset::new();
        ds.insert_str(
            Tag::SOP_CLASS_UID,
            Vr::Ui,
            crate::uids::COMPREHENSIVE_SR_STORAGE,
        );
        let mut item = Dataset::new();
        item.insert_str(Tag::VALUE_TYPE, Vr::Cs, "TEXT");
        item.insert_str(Tag::TEXT_VALUE, Vr::Ut, "Jane Doe was seen on 2026-01-01");
        ds.insert_sequence(Tag::CONTENT_SEQUENCE).push(item);

        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();

        let content = ds
            .try_get_sequence(Tag::CONTENT_SEQUENCE)
            .expect("the empty sequence stays");
        assert!(
            content.items().is_empty(),
            "SR ContentSequence must be emptied, got {:?}",
            content.items()
        );
    }

    #[test]
    fn a_custom_profile_without_a_patient_row_still_gets_the_override() {
        // FEAT-12: fo-dicom applies Profile.PatientName/PatientID independently
        // of the profile action, so an unmatched tag is still overridden.
        let profile = SecurityProfile::parse("0008,0060;K\n").unwrap();
        let anon = Anonymizer::with_profile(profile, AnonOption::BasicProfile)
            .with_patient("Alias^Name", "PAT-42");
        let mut ds = dataset();
        anon.anonymize_in_place(&mut ds).unwrap();
        assert_eq!(
            ds.try_get_str(Tag::PATIENT_NAME).as_deref(),
            Some("Alias^Name")
        );
        assert_eq!(ds.try_get_str(Tag::PATIENT_ID).as_deref(), Some("PAT-42"));
    }

    #[test]
    fn the_clone_variant_leaves_the_original_untouched() {
        let ds = dataset();
        let anonymized = Anonymizer::basic().anonymize(&ds).unwrap();
        assert_eq!(
            ds.try_get_str(Tag::PATIENT_NAME).as_deref(),
            Some("Doe^John")
        );
        assert_eq!(
            anonymized.try_get_str(Tag::PATIENT_NAME).as_deref(),
            Some("")
        );
    }

    #[test]
    fn a_date_tag_marked_z_is_emptied_and_one_marked_x_is_removed() {
        // cross-checked against fo-dicom's DicomAnonymizerTest:
        // StudyDate (0008,0020)=Z → empty; SeriesDate (0008,0021)=X/D → removed
        let mut ds = Dataset::new();
        ds.insert_str(Tag::STUDY_DATE, Vr::Da, "20260101"); // StudyDate: Z
        ds.insert_str(Tag::SERIES_DATE, Vr::Da, "20260102"); // SeriesDate: X/D
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();

        assert_eq!(
            ds.try_get_str(Tag::STUDY_DATE).as_deref(),
            Some(""),
            "StudyDate blanked"
        );
        assert!(
            ds.try_get_str(Tag::SERIES_DATE).is_none(),
            "SeriesDate removed"
        );
    }

    #[test]
    fn two_identical_source_uids_map_to_one_replacement() {
        // fo-dicom keeps a per-run map so consistency holds even across items
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4"); // SOPInstanceUID
        ds.insert_str(Tag::REFERENCED_SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4"); // ReferencedSOPInstanceUID
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        let a = ds.try_get_str(Tag::SOP_INSTANCE_UID).unwrap();
        let b = ds.try_get_str(Tag::REFERENCED_SOP_INSTANCE_UID).unwrap();
        assert_eq!(
            a, b,
            "the same UID must map consistently even across different tags"
        );
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
        child.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Nested^Name");
        ds.insert_sequence(Tag::REFERENCED_STUDY_SEQUENCE)
            .push(child);

        anon.anonymize_in_place(&mut ds).unwrap();
        assert!(
            ds.try_get_sequence(Tag::REFERENCED_STUDY_SEQUENCE)
                .is_none(),
            "an SQ marked X must be removed entirely"
        );
    }

    #[test]
    fn a_sequence_marked_keep_still_recurses_into_children() {
        // fo-dicom recurses when the action is null or K; K keeps the sequence
        // itself but cleans its children ("cleaned for sequences").
        let csv = "0008,1110;K\n0010,0020;Z\n";
        let profile = SecurityProfile::parse(csv).unwrap();
        let anon = Anonymizer::with_profile(profile, AnonOption::BasicProfile);

        let mut ds = Dataset::new();
        let mut child = Dataset::new();
        child.insert_str(Tag::PATIENT_ID, Vr::Lo, "PII"); // PatientID: Z under basic
        ds.insert_sequence(Tag::REFERENCED_STUDY_SEQUENCE)
            .push(child);

        anon.anonymize_in_place(&mut ds).unwrap();
        let seq = ds
            .try_get_sequence(Tag::REFERENCED_STUDY_SEQUENCE)
            .expect("sequence is kept");
        assert_eq!(
            seq.items()[0].try_get_str(Tag::PATIENT_ID).as_deref(),
            Some(""),
            "the child PatientID is still anonymized"
        );
    }

    #[test]
    fn anonymizing_a_file_syncs_the_media_storage_uid() {
        let mut file = DicomFile::from_dataset(dataset());
        file.file_meta
            .insert_str(Tag::MEDIA_STORAGE_SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4");
        Anonymizer::basic()
            .anonymize_file_in_place(&mut file)
            .unwrap();
        let sop = file.dataset.try_get_str(Tag::SOP_INSTANCE_UID).unwrap();
        let meta = file
            .file_meta
            .try_get_str(Tag::MEDIA_STORAGE_SOP_INSTANCE_UID)
            .unwrap();
        assert_eq!(
            meta, sop,
            "file meta must track the anonymized instance UID"
        );
    }

    #[test]
    fn the_uid_map_lives_on_the_anonymizer_not_on_the_call() {
        // fo-dicom keeps ReplacedUIDs on the anonymizer so a study's files stay
        // one study after de-identification
        let anon = Anonymizer::basic();
        let mut first = Dataset::new();
        first.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, "1.2.3.4");
        anon.anonymize_in_place(&mut first).unwrap();
        let mut second = Dataset::new();
        second.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, "1.2.3.4");
        anon.anonymize_in_place(&mut second).unwrap();

        let a = first.try_get_str(Tag::STUDY_INSTANCE_UID).unwrap();
        let b = second.try_get_str(Tag::STUDY_INSTANCE_UID).unwrap();
        assert_ne!(a, "1.2.3.4", "the UID is replaced");
        assert_eq!(a, b, "one source UID keeps one replacement across calls");
        assert_eq!(
            anon.replaced_uids().get("1.2.3.4"),
            Some(&a),
            "the map is readable"
        );

        // a second anonymizer starts with an empty map
        let other = Anonymizer::basic();
        assert!(other.replaced_uids().is_empty());
        let mut third = Dataset::new();
        third.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, "1.2.3.4");
        other.anonymize_in_place(&mut third).unwrap();
        assert_ne!(
            third.try_get_str(Tag::STUDY_INSTANCE_UID).unwrap(),
            a,
            "instances do not share a map"
        );
    }

    #[test]
    fn the_clean_graph_option_cleans_private_curve_tags() {
        // CleanGraph must reach the 50xx/60xx rows even though the leading
        // odd-private-group row matches those tags first
        let anon = Anonymizer::with_profile(SecurityProfile::basic(), AnonOption::CleanGraph);
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x5011, 0x1234), Vr::Lo, "raw curve data");
        anon.anonymize_in_place(&mut ds).unwrap();
        assert_eq!(
            ds.try_get_str(Tag::new(0x5011, 0x1234)).as_deref(),
            Some("ANONYMOUS"),
            "a CleanGraph-tagged private element is cleaned, not left intact"
        );
    }

    #[test]
    fn blanking_follows_the_vr() {
        let profile = SecurityProfile::parse(
            "0008,0020;D\n0008,0021;D\n0008,0022;D\n0008,002a;D\n0008,0031;D\n\
             0028,0010;D\n0028,0011;Z\n0008,0070;D\n0018,0050;D\n0020,0013;D\n\
             7FE0,0010;D\n7FE0,0011;Z\n",
        )
        .unwrap();
        let anon = Anonymizer::with_profile(profile, AnonOption::BasicProfile);
        let mut ds = Dataset::new();
        ds.insert_str(Tag::STUDY_DATE, Vr::Da, "20200101");
        ds.insert_str(Tag::SERIES_DATE, Vr::Da, "20200101");
        ds.insert_str(Tag::ACQUISITION_DATE_TIME, Vr::Dt, "20200101120000");
        ds.insert_str(Tag::SERIES_TIME, Vr::Tm, "120000");
        ds.insert_value(Tag::ROWS, 512u16);
        ds.insert_value(Tag::COLUMNS, 512u16);
        ds.insert_str(Tag::MANUFACTURER, Vr::Lo, "ACME");
        ds.insert_str(Tag::SLICE_THICKNESS, Vr::Ds, "1.5");
        ds.insert_str(Tag::INSTANCE_NUMBER, Vr::Is, "7");
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vec![1, 2, 3, 4])),
        )));
        anon.anonymize_in_place(&mut ds).unwrap();

        assert_eq!(ds.try_get_str(Tag::STUDY_DATE).as_deref(), Some("19000101"));
        assert_eq!(
            ds.try_get_str(Tag::ACQUISITION_DATE_TIME).as_deref(),
            Some("19000101000000")
        );
        assert_eq!(ds.try_get_str(Tag::SERIES_TIME).as_deref(), Some("000000"));
        assert_eq!(
            ds.try_get_str(Tag::MANUFACTURER).as_deref(),
            Some("ANONYMOUS"),
            "a generic string gets the dummy word"
        );
        assert_eq!(
            ds.try_get_str(Tag::SLICE_THICKNESS).as_deref(),
            Some("0"),
            "DS needs a numeric dummy"
        );
        assert_eq!(
            ds.try_get_str(Tag::INSTANCE_NUMBER).as_deref(),
            Some("0"),
            "IS needs a numeric dummy"
        );
        // binary numeric D writes one zero value (2 bytes for US)
        assert_eq!(
            ds.get_element(Tag::ROWS)
                .unwrap()
                .value()
                .as_buffer()
                .unwrap()
                .data()
                .unwrap()
                .as_ref(),
            &[0u8, 0]
        );
        // binary Z is an empty element
        assert!(ds.get_element(Tag::COLUMNS).unwrap().is_empty());
        // Other* is always empty, even for D
        assert!(ds.get_element(Tag::PIXEL_DATA).unwrap().is_empty());
    }

    #[test]
    fn an_empty_uid_is_not_replaced() {
        let mut ds = dataset();
        ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "");
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();
        assert_eq!(ds.try_get_str(Tag::SOP_INSTANCE_UID).as_deref(), Some(""));
    }

    #[test]
    fn anonymize_file_returns_a_clean_clone_and_syncs_the_meta() {
        let mut file = crate::io::file::DicomFile::from_dataset(dataset());
        let anon = Anonymizer::basic();
        let cloned = anon.anonymize_file(&file).unwrap();
        assert_eq!(
            cloned.dataset.try_get_str(Tag::PATIENT_ID).as_deref(),
            Some("")
        );
        assert_eq!(
            file.dataset.try_get_str(Tag::PATIENT_ID).as_deref(),
            Some("12345"),
            "the source file is untouched"
        );

        file.file_meta.remove(MEDIA_STORAGE_SOP_INSTANCE_UID);
        anon.anonymize_file_in_place(&mut file).unwrap();
        assert_eq!(
            file.dataset.try_get_str(Tag::PATIENT_NAME).as_deref(),
            Some("")
        );
    }

    #[test]
    fn clean_on_a_binary_value_blanks_it() {
        let profile = SecurityProfile::parse("0028,0010;C\n").unwrap();
        let anon = Anonymizer::with_profile(profile, AnonOption::BasicProfile);
        let mut ds = Dataset::new();
        ds.insert_value(Tag::ROWS, 512u16);
        anon.anonymize_in_place(&mut ds).unwrap();
        assert_eq!(
            ds.get_element(Tag::ROWS)
                .unwrap()
                .value()
                .as_buffer()
                .unwrap()
                .data()
                .unwrap()
                .as_ref(),
            &[0u8, 0]
        );
    }
}
