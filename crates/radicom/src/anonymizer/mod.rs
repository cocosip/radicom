//! PS3.15 de-identification engine (ANON-01; docs/architecture.md §12).
//!
//! Applies a `SecurityProfile` to a dataset: walk every element, resolve the
//! action for its tag under the chosen option, and mutate. UIDs are rewritten
//! consistently through a map that lives on the `Anonymizer` (fo-dicom's
//! `ReplacedUIDs`), so the same old UID maps to the same new one across every
//! dataset and file anonymized by that instance; a file's media-storage SOP
//! instance UID is kept in sync with the dataset's SOP instance UID (§12.5).

use crate::anonymizer::profile::{Action, SecurityProfile};
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

pub use profile::{AnonOption, AnonOptions};

/// (0010,0010) PatientName.
const PATIENT_NAME: Tag = Tag::PATIENT_NAME;
/// (0010,0020) PatientID.
const PATIENT_ID: Tag = Tag::PATIENT_ID;
/// (0008,0018) SOPInstanceUID.
const SOP_INSTANCE_UID: Tag = Tag::SOP_INSTANCE_UID;
/// (0002,0003) MediaStorageSOPInstanceUID.
const MEDIA_STORAGE_SOP_INSTANCE_UID: Tag = Tag::MEDIA_STORAGE_SOP_INSTANCE_UID;

/// Anonymizer that applies a profile under one option set (§12.5).
#[derive(Debug)]
pub struct Anonymizer {
    profile: SecurityProfile,
    /// The composed PS3.15 option set; the Basic Profile is the baseline and any
    /// further enabled option overrides it per attribute (§12.5).
    options: AnonOptions,
    patient_name: Option<String>,
    patient_id: Option<String>,
    /// Original → replacement UID, kept across calls (fo-dicom `ReplacedUIDs`);
    /// a `Mutex` so the methods can stay `&self`. Deliberately **not** shared
    /// across clones: each anonymizer instance keeps its own map.
    replaced: Arc<Mutex<HashMap<String, String>>>,
}

impl Clone for Anonymizer {
    /// A clone is an independent instance with a fresh, empty UID map, matching
    /// the documented "two instances never share it" guarantee (§12.5).
    fn clone(&self) -> Self {
        Anonymizer {
            profile: self.profile.clone(),
            options: self.options,
            patient_name: self.patient_name.clone(),
            patient_id: self.patient_id.clone(),
            replaced: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Anonymizer {
    /// An anonymizer over the built-in PS3.15 Basic Profile.
    pub fn basic() -> Self {
        Anonymizer::with_profile(SecurityProfile::basic(), AnonOptions::BASIC_PROFILE)
    }

    /// Uses the built-in PS3.15 profile under a combination of options.
    pub fn with_options(options: impl Into<AnonOptions>) -> Self {
        Anonymizer::with_profile(SecurityProfile::basic(), options)
    }

    /// Uses a custom profile under a chosen option set.
    ///
    /// Accepts either a single [`AnonOption`] (which implies `BasicProfile | it`,
    /// fo-dicom style, so the baseline de-identification always runs) or a
    /// composed [`AnonOptions`] such as
    /// `AnonOptions::BASIC_PROFILE | AnonOptions::bit(AnonOption::RetainUids)`.
    pub fn with_profile(profile: SecurityProfile, options: impl Into<AnonOptions>) -> Self {
        Anonymizer {
            profile,
            options: options.into(),
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
    ///
    /// With [`AnonOptions::NONE`] no action runs, so nothing is anonymized and
    /// the de-identification declarations are not written either: a dataset
    /// that still carries its PHI must never claim `PatientIdentityRemoved=YES`
    /// (PS3.15 E.1.1).
    ///
    /// The same rule applies to an options-only run: [`AnonOptions::bit`] does
    /// not add the Basic Profile baseline, so if the caller selected no baseline
    /// column nothing removed the patient identity and the declarations stay
    /// unwritten. Only the Basic Profile column makes that claim true.
    pub fn anonymize_in_place(&self, dataset: &mut Dataset) -> Result<(), Error> {
        if self.options.is_empty() {
            return Ok(());
        }
        let mut replaced = lock(&self.replaced);
        self.walk(dataset, &mut replaced)?;
        if !self.options.contains(AnonOption::BasicProfile) {
            return Ok(());
        }
        // PS3.15 E.1.1: a de-identified dataset declares that it has been
        // de-identified and by which method. Both are required output of the
        // Basic Profile (they are what tells a reader the PHI is gone).
        dataset.insert_str(Tag::PATIENT_IDENTITY_REMOVED, Vr::Cs, "YES");
        dataset.insert_str(
            Tag::DEIDENTIFICATION_METHOD,
            Vr::Lo,
            "RADICOM Basic Profile",
        );
        Ok(())
    }

    /// Anonymizes a file in place, replacing the identifying file meta.
    ///
    /// Like [`Self::anonymize_in_place`], [`AnonOptions::NONE`] is a no-op for
    /// the whole file, file meta included.
    pub fn anonymize_file_in_place(&self, file: &mut DicomFile) -> Result<(), Error> {
        if self.options.is_empty() {
            return Ok(());
        }
        self.anonymize_in_place(&mut file.dataset)?;
        // PS3.10 Table 7.1-1 / PS3.15 E.1.1: the file-meta instance UID must
        // equal the (now anonymized) dataset SOP instance UID, whether or not
        // the source file already carried one.
        if let Some(sop_instance) = file.dataset.try_get_str(SOP_INSTANCE_UID) {
            file.file_meta
                .insert_str(MEDIA_STORAGE_SOP_INSTANCE_UID, Vr::Ui, sop_instance);
        }
        // PS3.15 E.1.1 item 7: the File Meta Information can identify the source
        // (AE titles, private information), so those entries are replaced by the
        // de-identifier's own description; the writer forces the implementation
        // fields and the media-storage UIDs on save. This is a Basic Profile
        // step: an options-only run did not de-identify, so it must not rewrite
        // the source's meta either.
        if self.options.contains(AnonOption::BasicProfile) {
            for tag in [
                Tag::SOURCE_APPLICATION_ENTITY_TITLE,
                Tag::SENDING_APPLICATION_ENTITY_TITLE,
                Tag::RECEIVING_APPLICATION_ENTITY_TITLE,
                Tag::PRIVATE_INFORMATION_CREATOR_UID,
                Tag::PRIVATE_INFORMATION,
            ] {
                file.file_meta.remove(&tag);
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
            let action = self.profile.action_for_tag_options(&tag, self.options);

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
                // An encapsulated fragment sequence has no VR-shaped value to
                // blank: D/Z/U/C replace it with an empty sequence of the same
                // tag and VR, so no fragment bytes (an icon, a private payload)
                // survive. Skipping it left the payload in place.
                if matches!(dataset.item(&tag), Some(DicomItem::Fragments(_))) {
                    if action != Action::Keep {
                        let vr = dataset.item(&tag).map(|item| item.vr()).unwrap_or(Vr::Ob);
                        dataset.insert_or_update(DicomItem::Fragments(
                            crate::model::item::DicomFragmentSequence::new(tag.clone(), vr),
                        ));
                    }
                    self.apply_patient_override(dataset, &tag);
                    continue;
                }
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
                    Action::Clean | Action::Uid => self.blank(dataset, &tag, vr, true)?,
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

        // date/time: under RetainLongModifDates the value moves by the documented
        // constant offset so intervals survive (PS3.15 E.3.6); otherwise fo-dicom
        // writes an empty (Z) or MinValue (D) value.
        if matches!(vr, Vr::Da | Vr::Dt | Vr::Tm) {
            if non_zero {
                if self.options.contains(AnonOption::RetainLongModifDates) {
                    if let Some(shifted) = dataset
                        .try_get_str(&tag)
                        .and_then(|text| shift_modified_date_time(&text, vr))
                    {
                        dataset.insert_str(tag, vr, shifted);
                        return Ok(());
                    }
                }
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
            } else if vr == Vr::As {
                // AS is nnnD/W/M/Y; "ANONYMOUS" is not a legal age value.
                dataset.insert_str(tag, vr, "000Y");
            } else if vr == Vr::Ur {
                // UR is a URI; "ANONYMOUS" has no scheme. A reserved TLD keeps it
                // syntactically valid without pointing at a real endpoint.
                dataset.insert_str(tag, vr, "https://example.invalid/anonymous");
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

/// Days subtracted from a DA/DT under Retain Longitudinal Temporal Information
/// Modified Dates (PS3.15 E.3.6). The offset is constant so intervals between
/// dates survive; 3653 days is about ten years and keeps shifted dates plausible.
const MODIFIED_DATE_SHIFT_DAYS: i64 = 3653;

/// Seconds added to a TM under the same option, modulo 24 h. A constant offset
/// keeps relative times intact across a study.
const MODIFIED_TIME_SHIFT_SECONDS: i64 = 3661;

/// Shifts one DA/DT/TM value by the modified-dates constants, or `None` when the
/// value cannot be parsed (the caller then writes the old dummy).
fn shift_modified_date_time(text: &str, vr: Vr) -> Option<String> {
    match vr {
        Vr::Da => shift_da(text),
        Vr::Dt => shift_dt(text),
        Vr::Tm => shift_tm(text),
        _ => None,
    }
}

/// Shifts an eight-digit DA value by [`MODIFIED_DATE_SHIFT_DAYS`].
fn shift_da(text: &str) -> Option<String> {
    let (year, month, day) = parse_ymd(text.trim())?;
    shifted_date(year, month, day)
}

/// Shifts the leading date of a DT value, leaving its time, fraction and UTC
/// offset untouched. A DT shorter than a full date cannot be shifted safely.
fn shift_dt(text: &str) -> Option<String> {
    let text = text.trim();
    let (year, month, day) = parse_ymd(text.get(..8)?)?;
    let shifted = shifted_date(year, month, day)?;
    Some(format!("{shifted}{}", &text[8..]))
}

/// Shifts a TM value by [`MODIFIED_TIME_SHIFT_SECONDS`] modulo 24 h, keeping the
/// original precision and any fractional seconds.
fn shift_tm(text: &str) -> Option<String> {
    let text = text.trim();
    let (body, fraction) = match text.split_once('.') {
        Some((body, fraction)) => (body, Some(fraction)),
        None => (text, None),
    };
    let digits = |value: &str| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
    if !digits(body) || !matches!(body.len(), 2 | 4 | 6) {
        return None;
    }
    let hours: i64 = body.get(0..2)?.parse().ok()?;
    let minutes: i64 = match body.get(2..4) {
        Some(value) => value.parse().ok()?,
        None => 0,
    };
    let seconds: i64 = match body.get(4..6) {
        Some(value) => value.parse().ok()?,
        None => 0,
    };
    if hours > 23 || minutes > 59 || seconds > 60 {
        return None;
    }
    if let Some(fraction) = fraction {
        // A fraction is only legal after seconds (PS3.6 TM).
        if body.len() != 6 || fraction.is_empty() || fraction.len() > 6 || !digits(fraction) {
            return None;
        }
    }
    let total =
        (hours * 3600 + minutes * 60 + seconds + MODIFIED_TIME_SHIFT_SECONDS).rem_euclid(24 * 3600);
    let (hours, rest) = (total / 3600, total % 3600);
    let (minutes, seconds) = (rest / 60, rest % 60);
    let mut out = match body.len() {
        2 => format!("{hours:02}"),
        4 => format!("{hours:02}{minutes:02}"),
        _ => format!("{hours:02}{minutes:02}{seconds:02}"),
    };
    if let Some(fraction) = fraction {
        out.push('.');
        out.push_str(fraction);
    }
    Some(out)
}

/// Parses a `YYYYMMDD` date, rejecting impossible calendar days.
fn parse_ymd(text: &str) -> Option<(i64, i64, i64)> {
    if text.len() != 8 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(4..6)?.parse().ok()?;
    let day: i64 = text.get(6..8)?.parse().ok()?;
    if year < 1 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    // The round trip rejects 20240231: the civil conversion normalises it.
    if civil_from_days(days) != (year, month, day) {
        return None;
    }
    Some((year, month, day))
}

/// Shifts a date by the fixed day count, or `None` when it leaves years 1..=9999.
fn shifted_date(year: i64, month: i64, day: i64) -> Option<String> {
    let days = days_from_civil(year, month, day).checked_sub(MODIFIED_DATE_SHIFT_DAYS)?;
    let (year, month, day) = civil_from_days(days);
    if !(1..=9999).contains(&year) {
        return None;
    }
    Some(format!("{year:04}{month:02}{day:02}"))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`, PS3.5 A.1's calendar without leap-second trivia).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (year + i64::from(month <= 2), month, day)
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
    use crate::anonymizer::profile::AnonOption;
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
    fn the_none_option_neither_anonymizes_nor_claims_deidentification() {
        let mut ds = dataset();
        ds.insert_str(Tag::PATIENT_IDENTITY_REMOVED, Vr::Cs, "NO");
        Anonymizer::with_profile(SecurityProfile::basic(), AnonOptions::NONE)
            .anonymize_in_place(&mut ds)
            .unwrap();
        assert_eq!(
            ds.try_get_str(Tag::PATIENT_NAME).as_deref(),
            Some("Doe^John"),
            "NONE keeps the plaintext"
        );
        assert_eq!(
            ds.try_get_str(Tag::PATIENT_IDENTITY_REMOVED).as_deref(),
            Some("NO"),
            "NONE must not claim that the PHI is gone"
        );
        assert!(
            ds.item(&Tag::DEIDENTIFICATION_METHOD).is_none(),
            "NONE must not write a de-identification method"
        );
    }

    #[test]
    fn an_options_only_run_does_not_claim_deidentification() {
        // AnonOptions::bit does not add the Basic Profile baseline, so nothing
        // removed the patient identity: the PHI stays and must not be declared
        // gone (PS3.15 E.1.1).
        let mut ds = dataset();
        Anonymizer::with_options(AnonOptions::bit(AnonOption::RetainUids))
            .anonymize_in_place(&mut ds)
            .unwrap();
        assert_eq!(
            ds.try_get_str(Tag::PATIENT_NAME).as_deref(),
            Some("Doe^John"),
            "no baseline column runs, so the name is kept"
        );
        assert!(
            ds.item(&Tag::PATIENT_IDENTITY_REMOVED).is_none(),
            "an options-only run must not claim that the PHI is gone"
        );
        assert!(
            ds.item(&Tag::DEIDENTIFICATION_METHOD).is_none(),
            "an options-only run must not write a de-identification method"
        );

        // The file-meta cleanup is part of the same Basic Profile step, so an
        // options-only run must not rewrite the source's AE titles either.
        let mut file = DicomFile::from_dataset(dataset());
        file.file_meta
            .insert_str(Tag::SOURCE_APPLICATION_ENTITY_TITLE, Vr::Ae, "SCU");
        Anonymizer::with_options(AnonOptions::bit(AnonOption::RetainUids))
            .anonymize_file_in_place(&mut file)
            .unwrap();
        assert_eq!(
            file.file_meta
                .try_get_str(Tag::SOURCE_APPLICATION_ENTITY_TITLE)
                .as_deref(),
            Some("SCU"),
            "an options-only run leaves the file meta alone"
        );
    }

    #[test]
    fn the_none_option_leaves_the_file_meta_alone() {
        let mut file = DicomFile::from_dataset(dataset());
        file.file_meta
            .insert_str(Tag::SOURCE_APPLICATION_ENTITY_TITLE, Vr::Ae, "SCU");
        Anonymizer::with_profile(SecurityProfile::basic(), AnonOptions::NONE)
            .anonymize_file_in_place(&mut file)
            .unwrap();
        assert_eq!(
            file.file_meta
                .try_get_str(Tag::SOURCE_APPLICATION_ENTITY_TITLE)
                .as_deref(),
            Some("SCU"),
            "nothing is anonymized, so the file meta stays as it was"
        );
        assert!(file.dataset.item(&Tag::PATIENT_IDENTITY_REMOVED).is_none());
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
    fn a_fragment_sequence_is_blanked_by_a_blanking_action() {
        // A private fragment tag matched by D must not keep its fragment bytes;
        // it becomes an empty fragment sequence of the same tag and VR.
        use crate::model::item::{DicomFragmentSequence, DicomItem};
        let tag = Tag::new(0x0009, 0x1001);
        let mut fragments = DicomFragmentSequence::new(tag, Vr::Ob);
        fragments.push_fragment(std::sync::Arc::new(
            crate::model::buffer::MemoryBuffer::from(vec![0xAAu8; 8]),
        ));
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Fragments(fragments));
        let profile = SecurityProfile::parse("0009,1001;D\n").unwrap();
        Anonymizer::with_profile(profile, AnonOptions::BASIC_PROFILE)
            .anonymize_in_place(&mut ds)
            .unwrap();
        let item = ds
            .item(&Tag::new(0x0009, 0x1001))
            .expect("the element stays");
        assert!(
            matches!(item, DicomItem::Fragments(_)),
            "it is still a fragment sequence"
        );
        assert!(item.is_empty_value(), "no fragment payload survives");
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
    fn a_clone_starts_with_its_own_empty_uid_map() {
        let original = Anonymizer::basic();
        let clone = original.clone();
        assert!(
            clone.replaced_uids().is_empty(),
            "a cloned anonymizer must not inherit the original's UID map (§12.5)"
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
        // odd-private-group row matches those tags first. A bare `AnonOption`
        // now implies the Basic Profile (FJ-01), and fo-dicom's row order then
        // makes the odd-private row's Basic action (remove) win; the column is
        // requested alone here, which is the fo-dicom `CleanGraph`-only call.
        let anon = Anonymizer::with_profile(
            SecurityProfile::basic(),
            AnonOptions::bit(AnonOption::CleanGraph),
        );
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
    fn modified_dates_shift_consistently_instead_of_blanking() {
        // PS3.15 E.3.6: RetainLongModifDates must move a date by a constant offset
        // so intervals survive. The old Clean path wrote 19000101, destroying them.
        let options =
            AnonOptions::BASIC_PROFILE | AnonOptions::bit(AnonOption::RetainLongModifDates);
        let anon = Anonymizer::with_profile(SecurityProfile::basic(), options);
        let mut early = Dataset::new();
        early.insert_str(Tag::STUDY_DATE, Vr::Da, "20240101");
        early.insert_str(Tag::STUDY_TIME, Vr::Tm, "120000.500000");
        let mut late = Dataset::new();
        late.insert_str(Tag::STUDY_DATE, Vr::Da, "20240301");
        anon.anonymize_in_place(&mut early).unwrap();
        anon.anonymize_in_place(&mut late).unwrap();

        let early_date = early.try_get_str(Tag::STUDY_DATE).unwrap();
        let late_date = late.try_get_str(Tag::STUDY_DATE).unwrap();
        assert_ne!(early_date, "20240101", "the date is modified");
        assert_ne!(late_date, "20240301", "the date is modified");
        assert_eq!(
            test_day_number(&late_date) - test_day_number(&early_date),
            60,
            "the 60-day interval is preserved"
        );
        // DT keeps its time, fraction and offset; only the date part moves
        let mut datetime = Dataset::new();
        datetime.insert_str(
            Tag::ACQUISITION_DATE_TIME,
            Vr::Dt,
            "20240101120000.123456-0500",
        );
        anon.anonymize_in_place(&mut datetime).unwrap();
        let dt = datetime.try_get_str(Tag::ACQUISITION_DATE_TIME).unwrap();
        assert!(
            dt.ends_with("120000.123456-0500"),
            "time/fraction/offset intact: {dt}"
        );
        assert_ne!(&dt[..8], "20240101", "the date part moved");
        // TM shifts by the same amount and keeps the fraction
        let time = early.try_get_str(Tag::STUDY_TIME).unwrap();
        assert_ne!(time, "120000.500000");
        assert!(time.ends_with(".500000"), "fraction preserved: {time}");
        let mut later = Dataset::new();
        later.insert_str(Tag::STUDY_TIME, Vr::Tm, "130000.500000");
        anon.anonymize_in_place(&mut later).unwrap();
        let shifted_later = later.try_get_str(Tag::STUDY_TIME).unwrap();
        let early_seconds = test_time_seconds(&time);
        let later_seconds = test_time_seconds(&shifted_later);
        assert_eq!(
            (later_seconds - early_seconds).rem_euclid(24 * 3600),
            3600,
            "equal times shift by equal amounts"
        );

        // The option being off keeps the old dummy behaviour
        let mut plain = Dataset::new();
        plain.insert_str(Tag::STUDY_DATE, Vr::Da, "20240101");
        Anonymizer::basic().anonymize_in_place(&mut plain).unwrap();
        assert_eq!(plain.try_get_str(Tag::STUDY_DATE).as_deref(), Some(""));
    }

    /// Days since 1-1-1 in the proleptic Gregorian calendar; independent of the
    /// production conversion so the interval check is not self-confirming.
    fn test_day_number(text: &str) -> i64 {
        let year: i64 = text[0..4].parse().unwrap();
        let month: i64 = text[4..6].parse().unwrap();
        let day: i64 = text[6..8].parse().unwrap();
        let y = year - 1;
        let mut days = y * 365 + y / 4 - y / 100 + y / 400;
        const BEFORE_MONTH: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
        days += BEFORE_MONTH[(month - 1) as usize];
        if month > 2 && ((year % 4 == 0 && year % 100 != 0) || year % 400 == 0) {
            days += 1;
        }
        days + day
    }

    /// Seconds since midnight of an `HHMMSS` TM value.
    fn test_time_seconds(text: &str) -> i64 {
        let body = text.split('.').next().unwrap_or(text);
        let hh: i64 = body[0..2].parse().unwrap();
        let mm: i64 = body[2..4].parse().unwrap();
        let ss: i64 = body[4..6].parse().unwrap();
        hh * 3600 + mm * 60 + ss
    }

    #[test]
    fn generic_dummies_respect_the_vr_syntax() {
        // AS is nnnD/W/M/Y and UR is a URI: the generic "ANONYMOUS" word is not
        // legal for either, so each needs its own valid dummy.
        let profile = SecurityProfile::parse("0010,1010;D\n0008,1190;D\n0010,0020;D\n").unwrap();
        let anon = Anonymizer::with_profile(profile, AnonOption::BasicProfile);
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_AGE, Vr::As, "042Y");
        ds.insert_str(Tag::RETRIEVE_URL, Vr::Ur, "https://pacs.example/studies/1");
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "12345");
        anon.anonymize_in_place(&mut ds).unwrap();
        assert_eq!(ds.try_get_str(Tag::PATIENT_AGE).as_deref(), Some("000Y"));
        let url = ds.try_get_str(Tag::RETRIEVE_URL).unwrap();
        assert!(url.contains("://"), "a syntactically valid URI: {url}");
        assert_eq!(
            ds.try_get_str(Tag::PATIENT_ID).as_deref(),
            Some("ANONYMOUS"),
            "a generic string keeps its existing dummy"
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
    fn anonymizing_a_file_replaces_the_identifying_meta_and_declares_the_method() {
        let mut file = crate::io::file::DicomFile::from_dataset(dataset());
        file.file_meta
            .insert_str(Tag::SOURCE_APPLICATION_ENTITY_TITLE, Vr::Ae, "SECRET_AE");
        file.file_meta
            .insert_str(Tag::IMPLEMENTATION_VERSION_NAME, Vr::Sh, "VENDOR_1");
        file.file_meta
            .insert_value(Tag::PRIVATE_INFORMATION, vec![1u8, 2, 3, 4]);
        Anonymizer::basic()
            .anonymize_file_in_place(&mut file)
            .unwrap();
        // PS3.15 E.1.1 item 7: the source AE title and private info are gone.
        assert!(!file
            .file_meta
            .contains(&Tag::SOURCE_APPLICATION_ENTITY_TITLE));
        assert!(!file.file_meta.contains(&Tag::PRIVATE_INFORMATION));
        // PS3.15 E.1.1 item 6: the dataset declares the de-identification.
        assert_eq!(
            file.dataset
                .try_get_str(Tag::PATIENT_IDENTITY_REMOVED)
                .as_deref(),
            Some("YES")
        );
        assert!(
            file.dataset
                .try_get_str(Tag::DEIDENTIFICATION_METHOD)
                .is_some(),
            "the method describes the profile that ran"
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

    #[test]
    fn a_retain_option_combined_with_the_basic_profile_keeps_the_device_serial() {
        // (0018,1000) DeviceSerialNumber: Basic Profile removes it, the
        // RetainDeviceIdent column keeps it; only the combination is correct.
        let mut ds = dataset();
        ds.insert_str(Tag::new(0x0018, 0x1000), Vr::Lo, "SN-42");

        let mut basic = ds.clone();
        Anonymizer::basic().anonymize_in_place(&mut basic).unwrap();
        assert!(
            !basic.contains(&Tag::new(0x0018, 0x1000)),
            "Basic Profile alone removes the device serial"
        );
        assert_eq!(
            basic.try_get_str(Tag::PATIENT_NAME).as_deref(),
            Some(""),
            "Basic Profile still blanks the patient name"
        );

        let combined = Anonymizer::with_options(
            AnonOptions::BASIC_PROFILE | AnonOptions::bit(AnonOption::RetainDeviceIdent),
        );
        let mut merged = ds.clone();
        combined.anonymize_in_place(&mut merged).unwrap();
        assert_eq!(
            merged.try_get_str(Tag::new(0x0018, 0x1000)).as_deref(),
            Some("SN-42"),
            "the retained column wins over the Basic Profile cell"
        );
        assert_eq!(
            merged.try_get_str(Tag::PATIENT_NAME).as_deref(),
            Some(""),
            "the Basic Profile still applies to tags the retain column does not name"
        );
    }

    #[test]
    fn the_single_option_constructor_still_matches_the_default_basic_profile() {
        // the pre-flags API (one AnonOption column) must behave identically; the
        // SOP instance UID is dropped because a U-action generates a random UID
        let plain = Anonymizer::basic();
        let single = Anonymizer::with_profile(SecurityProfile::basic(), AnonOption::BasicProfile);
        let mut base = dataset();
        base.remove(Tag::SOP_INSTANCE_UID);
        let mut a = base.clone();
        let mut b = base;
        plain.anonymize_in_place(&mut a).unwrap();
        single.anonymize_in_place(&mut b).unwrap();
        assert_eq!(a.to_dump_string(), b.to_dump_string());
    }
    /// FJ-01 regression: every single option still runs the Basic Profile, so
    /// PatientName and PatientID are de-identified (previously a non-Basic
    /// option selected only its own column and left the identifiers intact).
    #[test]
    fn every_option_still_blank_the_patient_identifiers() {
        for option in AnonOption::ALL {
            let anon = Anonymizer::with_profile(SecurityProfile::basic(), option);
            let mut ds = Dataset::new();
            ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
            ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "12345");
            anon.anonymize_in_place(&mut ds).unwrap();
            assert_eq!(
                ds.try_get_str(Tag::PATIENT_NAME).as_deref(),
                Some(""),
                "PatientName must be blanked under {option:?}"
            );
            assert_eq!(
                ds.try_get_str(Tag::PATIENT_ID).as_deref(),
                Some(""),
                "PatientID must be blanked under {option:?}"
            );
        }
    }

    /// FJ-01 regression: `BasicProfile | RetainUids` keeps a UID but still
    /// blanks PatientName. The combined form and the single-option call site
    /// (which now implies `BasicProfile | it`) must agree.
    #[test]
    fn retain_uids_keeps_uids_but_still_blanks_patient_name() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "12345");
        ds.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, "1.2.3.4");
        ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.5");

        let combined = AnonOptions::BASIC_PROFILE | AnonOptions::bit(AnonOption::RetainUids);
        Anonymizer::with_profile(SecurityProfile::basic(), combined)
            .anonymize_in_place(&mut ds)
            .unwrap();

        assert_eq!(
            ds.try_get_str(Tag::STUDY_INSTANCE_UID).as_deref(),
            Some("1.2.3.4"),
            "RetainUids keeps StudyInstanceUID"
        );
        assert_eq!(
            ds.try_get_str(Tag::SOP_INSTANCE_UID).as_deref(),
            Some("1.2.3.5"),
            "RetainUids keeps SOPInstanceUID"
        );
        assert_eq!(
            ds.try_get_str(Tag::PATIENT_NAME).as_deref(),
            Some(""),
            "the baseline still blanks PatientName"
        );
        assert_eq!(ds.try_get_str(Tag::PATIENT_ID).as_deref(), Some(""));

        // the single-option call site must behave the same (Baseline | RetainUids)
        let mut single = Dataset::new();
        single.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        single.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, "1.2.3.4");
        Anonymizer::with_profile(SecurityProfile::basic(), AnonOption::RetainUids)
            .anonymize_in_place(&mut single)
            .unwrap();
        assert_eq!(
            single.try_get_str(Tag::STUDY_INSTANCE_UID).as_deref(),
            Some("1.2.3.4")
        );
        assert_eq!(single.try_get_str(Tag::PATIENT_NAME).as_deref(), Some(""));
    }

    /// FJ-01 regression: `BasicProfile | CleanDesc` cleans StudyDescription,
    /// while the baseline still blanks PatientName.
    #[test]
    fn clean_desc_cleans_study_description_without_losing_the_baseline() {
        let options = AnonOptions::BASIC_PROFILE | AnonOptions::bit(AnonOption::CleanDesc);
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds.insert_str(Tag::STUDY_DESCRIPTION, Vr::Lo, "Chest CT");

        Anonymizer::with_profile(SecurityProfile::basic(), options)
            .anonymize_in_place(&mut ds)
            .unwrap();

        assert_eq!(
            ds.try_get_str(Tag::STUDY_DESCRIPTION).as_deref(),
            Some("ANONYMOUS"),
            "CleanDesc cleans StudyDescription"
        );
        assert_eq!(
            ds.try_get_str(Tag::PATIENT_NAME).as_deref(),
            Some(""),
            "the baseline blanks PatientName"
        );
    }

    /// FJ-01 regression: the default `basic()` still blanks identifiers and
    /// removes a description (Basic column X), i.e. nothing changed for callers.
    #[test]
    fn default_basic_profile_behaviour_is_unchanged() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "12345");
        ds.insert_str(Tag::STUDY_DESCRIPTION, Vr::Lo, "Chest CT");
        Anonymizer::basic().anonymize_in_place(&mut ds).unwrap();

        assert_eq!(ds.try_get_str(Tag::PATIENT_NAME).as_deref(), Some(""));
        assert_eq!(ds.try_get_str(Tag::PATIENT_ID).as_deref(), Some(""));
        assert!(
            ds.try_get_str(Tag::STUDY_DESCRIPTION).is_none(),
            "the Basic column removes StudyDescription"
        );
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::anonymizer::profile::SecurityProfile;

    #[test]
    fn a_fragment_item_with_an_action_is_skipped_and_left_intact() {
        let profile = SecurityProfile::parse("7FE0,0010;Z\n").unwrap();
        let anonymizer = Anonymizer::with_profile(profile, AnonOptions::BASIC_PROFILE);
        let mut ds = Dataset::new();
        ds.insert_fragments(Tag::PIXEL_DATA, Vr::Ob);
        anonymizer.anonymize_in_place(&mut ds).unwrap();
        assert!(matches!(
            ds.item(&Tag::PIXEL_DATA),
            Some(DicomItem::Fragments(_))
        ));
    }
}
