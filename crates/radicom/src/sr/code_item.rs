//! SR support types (SR-02; docs/01-architecture.md §12).
//!
//! `CodeItem` is the coded-entry triple that SR (and many other IODs) reuse.
//! PS3.3 §8.8 gives it a single tag set: `CodingSchemeDesignator (0008,0102)`,
//! `CodingSchemeVersion (0008,0103)` and `CodeMeaning (0008,0104)` are fixed,
//! while the code *value* picks one of three slots — `CodeValue (0008,0100)`
//! SH for values up to 16 bytes, `LongCodeValue (0008,0119)` UC for longer
//! ones, and `URNCodeValue (0008,0120)` UR for a URN.

use crate::error::{DataError, Error};
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::model::vr::Vr;

/// A coded entry: value + coding scheme (+ optional meaning/version) (§12.3).
///
/// Equality is derived, so it compares all four fields **including the meaning
/// text**; fo-dicom's `DicomCodeItem.Equals` compares only value, scheme and
/// version. This is deliberately stricter: two entries that describe the same
/// code differently are kept apart, which matters when a meaning is used as the
/// display text of a tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeItem {
    pub value: String,
    pub scheme: String,
    pub meaning: Option<String>,
    pub scheme_version: Option<String>,
}

impl CodeItem {
    pub fn new(value: impl Into<String>, scheme: impl Into<String>) -> Self {
        CodeItem {
            value: value.into(),
            scheme: scheme.into(),
            meaning: None,
            scheme_version: None,
        }
    }

    /// Builds a URN code item with its display meaning and no scheme metadata.
    pub fn from_urn(urn: impl Into<String>, meaning: impl Into<String>) -> Self {
        CodeItem {
            value: urn.into(),
            scheme: String::new(),
            meaning: Some(meaning.into()),
            scheme_version: None,
        }
    }

    pub fn with_meaning(mut self, meaning: impl Into<String>) -> Self {
        self.meaning = Some(meaning.into());
        self
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.scheme_version = Some(version.into());
        self
    }

    /// Writes this code into `dataset`, choosing the value slot by the code
    /// value's length and form (§12.3).
    pub fn write_into(&self, dataset: &mut Dataset) {
        for tag in [
            Tag::CODE_VALUE,
            Tag::LONG_CODE_VALUE,
            Tag::URN_CODE_VALUE,
            Tag::CODING_SCHEME_DESIGNATOR,
            Tag::CODE_MEANING,
            Tag::CODING_SCHEME_VERSION,
        ] {
            dataset.remove(tag);
        }
        let (value_tag, value_vr) = value_slot(&self.value);
        dataset.insert_str(value_tag, value_vr, self.value.clone());
        if !self.scheme.is_empty() {
            dataset.insert_str(Tag::CODING_SCHEME_DESIGNATOR, Vr::Sh, self.scheme.clone());
        }
        if let Some(meaning) = &self.meaning {
            if !meaning.is_empty() {
                dataset.insert_str(Tag::CODE_MEANING, Vr::Lo, meaning.clone());
            }
        }
        if let Some(version) = &self.scheme_version {
            if !version.is_empty() {
                dataset.insert_str(Tag::CODING_SCHEME_VERSION, Vr::Sh, version.clone());
            }
        }
    }

    /// Reads a code from the item's value slot (§12.3).
    pub fn read_from(dataset: &Dataset) -> Result<Self, Error> {
        let value = [Tag::CODE_VALUE, Tag::LONG_CODE_VALUE, Tag::URN_CODE_VALUE]
            .into_iter()
            .find_map(|tag| dataset.try_get_str(&tag));
        let Some(value) = value else {
            return Err(Error::Data(DataError::Other {
                message: "no coded entry found".into(),
            }));
        };
        Ok(CodeItem {
            value: value.trim().to_string(),
            scheme: dataset
                .try_get_str(&Tag::CODING_SCHEME_DESIGNATOR)
                .unwrap_or_default()
                .trim()
                .to_string(),
            meaning: dataset
                .try_get_str(&Tag::CODE_MEANING)
                .map(|m| m.trim().to_string()),
            scheme_version: dataset
                .try_get_str(&Tag::CODING_SCHEME_VERSION)
                .map(|v| v.trim().to_string()),
        })
    }

    /// Checks a complete coded entry, including mutually exclusive value fields.
    pub(crate) fn validate_dataset(dataset: &Dataset) -> Result<(), Error> {
        let slots = [Tag::CODE_VALUE, Tag::LONG_CODE_VALUE, Tag::URN_CODE_VALUE];
        let present: Vec<_> = slots
            .into_iter()
            .filter(|tag| dataset.contains(tag))
            .collect();
        let code = Self::read_from(dataset)?;
        if present.len() != 1
            || code.value.is_empty()
            || code
                .meaning
                .as_deref()
                .is_none_or(|meaning| meaning.trim().is_empty())
            || (present[0] != Tag::URN_CODE_VALUE && code.scheme.is_empty())
        {
            return Err(Error::Data(DataError::Other {
                message: "incomplete or ambiguous coded entry".into(),
            }));
        }
        for (tag, vr) in [
            (Tag::CODE_VALUE, Vr::Sh),
            (Tag::LONG_CODE_VALUE, Vr::Uc),
            (Tag::URN_CODE_VALUE, Vr::Ur),
            (Tag::CODING_SCHEME_DESIGNATOR, Vr::Sh),
            (Tag::CODE_MEANING, Vr::Lo),
            (Tag::CODING_SCHEME_VERSION, Vr::Sh),
        ] {
            if let Some(crate::model::item::DicomItem::Element(element)) = dataset.item(&tag) {
                let value = dataset.get_str(&tag)?;
                if element.vr() != vr
                    || value.trim().is_empty()
                    || (vr.is_multi_value() && value.contains('\\'))
                {
                    return Err(Error::Data(DataError::Other {
                        message: format!("incorrect coded entry VR or multiplicity for {tag}"),
                    }));
                }
                crate::validation::validate_single(vr, &value)
                    .map_err(|message| Error::Data(DataError::Other { message }))?;
            } else if dataset.contains(&tag) {
                return Err(Error::Data(DataError::Other {
                    message: format!("coded entry {tag} must be an element"),
                }));
            }
        }
        for item in dataset.items() {
            if let crate::model::item::DicomItem::Element(element) = item {
                if !matches!(
                    element.value(),
                    crate::model::element::ElementValue::Bytes(_)
                ) {
                    crate::validation::validate_element(element)?;
                }
            }
        }
        Ok(())
    }

    /// Wraps the code in a fresh one-item sequence under `sequence_tag`.
    pub fn to_sequence(&self, sequence_tag: Tag) -> Dataset {
        let mut item = Dataset::new();
        self.write_into(&mut item);
        let mut holder = Dataset::new();
        holder.insert_sequence(sequence_tag).push(item);
        holder
    }
}

/// The value slot for `value` (§12.3, PS3.3 §8.8).
///
/// A URN goes in `URNCodeValue` (UR); any other value longer than SH's 16
/// bytes cannot live in `CodeValue` (SH) and uses `LongCodeValue` (UC).
fn value_slot(value: &str) -> (Tag, Vr) {
    if is_urn(value) {
        (Tag::URN_CODE_VALUE, Vr::Ur)
    } else if value.len() > 16 {
        (Tag::LONG_CODE_VALUE, Vr::Uc)
    } else {
        (Tag::CODE_VALUE, Vr::Sh)
    }
}

fn is_urn(value: &str) -> bool {
    // fo-dicom `DicomCodeItem` treats any `urn:` **or** `http…` value as a URN
    value.len() >= 4
        && (value.as_bytes()[..4].eq_ignore_ascii_case(b"urn:")
            || value.as_bytes()[..4].eq_ignore_ascii_case(b"http"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // PS3.3 §8.8 tag numbers, spelled out so the assertions do not reuse the
    // implementation's own constants.
    const CODE_VALUE: Tag = Tag::new(0x0008, 0x0100);
    const CODING_SCHEME_DESIGNATOR: Tag = Tag::new(0x0008, 0x0102);
    const CODING_SCHEME_VERSION: Tag = Tag::new(0x0008, 0x0103);
    const CODE_MEANING: Tag = Tag::new(0x0008, 0x0104);
    const CONTEXT_GROUP_VERSION: Tag = Tag::new(0x0008, 0x0106);
    const LONG_CODE_VALUE: Tag = Tag::new(0x0008, 0x0119);
    const URN_CODE_VALUE: Tag = Tag::new(0x0008, 0x0120);

    fn vr_of(dataset: &Dataset, tag: Tag) -> Option<Vr> {
        match dataset.item(&tag) {
            Some(crate::model::item::DicomItem::Element(element)) => Some(element.vr()),
            _ => None,
        }
    }

    #[test]
    fn coded_entries_require_one_value_in_the_expected_vr() {
        let mut dataset = Dataset::new();
        CodeItem::new("A", "TEST")
            .with_meaning("Heading")
            .write_into(&mut dataset);
        CodeItem::validate_dataset(&dataset).unwrap();
        for (tag, vr, value) in [
            (Tag::CODE_VALUE, Vr::Sh, "A\\B"),
            (Tag::CODE_MEANING, Vr::Sh, "Heading"),
            (Tag::CODING_SCHEME_DESIGNATOR, Vr::Sh, ""),
        ] {
            let mut bad = dataset.clone();
            bad.insert_str(tag, vr, value);
            assert!(CodeItem::validate_dataset(&bad).is_err());
        }
        dataset.insert_str(Tag::LONG_CODE_VALUE, Vr::Uc, "another value");
        assert!(CodeItem::validate_dataset(&dataset).is_err());
        CodeItem::from_urn("urn:test:a", "URN heading").write_into(&mut dataset);
        CodeItem::validate_dataset(&dataset).unwrap();
        assert!(!dataset.contains(&Tag::CODE_VALUE) && !dataset.contains(&Tag::LONG_CODE_VALUE));
    }

    #[test]
    fn a_short_code_writes_code_value_with_its_scheme_tags() {
        let mut ds = Dataset::new();
        CodeItem::new("T-A0100", "SRT")
            .with_meaning("Chest")
            .with_version("1")
            .write_into(&mut ds);

        assert_eq!(ds.try_get_str(CODE_VALUE).as_deref(), Some("T-A0100"));
        assert_eq!(vr_of(&ds, CODE_VALUE), Some(Vr::Sh));
        assert_eq!(
            ds.try_get_str(CODING_SCHEME_DESIGNATOR).as_deref(),
            Some("SRT")
        );
        assert_eq!(vr_of(&ds, CODING_SCHEME_DESIGNATOR), Some(Vr::Sh));
        assert_eq!(ds.try_get_str(CODE_MEANING).as_deref(), Some("Chest"));
        assert_eq!(vr_of(&ds, CODE_MEANING), Some(Vr::Lo));
        assert_eq!(ds.try_get_str(CODING_SCHEME_VERSION).as_deref(), Some("1"));
        assert_eq!(vr_of(&ds, CODING_SCHEME_VERSION), Some(Vr::Sh));

        assert!(
            ds.try_get_str(LONG_CODE_VALUE).is_none(),
            "a short value stays in CodeValue"
        );
        assert!(ds.try_get_str(URN_CODE_VALUE).is_none());
        // (0008,0106) is ContextGroupVersion (DT), never a scheme version
        assert!(ds.try_get_str(CONTEXT_GROUP_VERSION).is_none());
    }

    #[test]
    fn a_value_longer_than_16_bytes_writes_long_code_value() {
        let mut ds = Dataset::new();
        // 17 bytes: one over the SH limit
        CodeItem::new("12345678901234567", "99DIN").write_into(&mut ds);
        assert_eq!(
            ds.try_get_str(LONG_CODE_VALUE).as_deref(),
            Some("12345678901234567")
        );
        assert_eq!(vr_of(&ds, LONG_CODE_VALUE), Some(Vr::Uc));
        assert!(
            ds.try_get_str(CODE_VALUE).is_none(),
            "SH cannot hold 17 bytes"
        );
        assert!(ds.try_get_str(URN_CODE_VALUE).is_none());
    }

    #[test]
    fn a_urn_writes_urn_code_value() {
        let mut ds = Dataset::new();
        CodeItem::new("urn:oid:1.2.3", "DCM").write_into(&mut ds);
        assert_eq!(
            ds.try_get_str(URN_CODE_VALUE).as_deref(),
            Some("urn:oid:1.2.3")
        );
        assert_eq!(vr_of(&ds, URN_CODE_VALUE), Some(Vr::Ur));
        assert!(ds.try_get_str(CODE_VALUE).is_none());
        assert!(ds.try_get_str(LONG_CODE_VALUE).is_none());
    }

    #[test]
    fn a_16_byte_value_still_fits_code_value() {
        let mut ds = Dataset::new();
        CodeItem::new("1234567890123456", "99DIN").write_into(&mut ds);
        assert_eq!(
            ds.try_get_str(CODE_VALUE).as_deref(),
            Some("1234567890123456")
        );
        assert!(ds.try_get_str(LONG_CODE_VALUE).is_none());
    }

    #[test]
    fn a_code_round_trips_through_a_dataset() {
        let original = CodeItem::new("T-A0100", "SRT")
            .with_meaning("Chest")
            .with_version("1");
        let mut ds = Dataset::new();
        original.write_into(&mut ds);
        let parsed = CodeItem::read_from(&ds).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn long_and_urn_values_round_trip() {
        for code in [
            CodeItem::new("12345678901234567", "99DIN"),
            CodeItem::new("urn:oid:1.2.3", "DCM"),
        ] {
            let mut ds = Dataset::new();
            code.write_into(&mut ds);
            assert_eq!(CodeItem::read_from(&ds).unwrap(), code);
        }
    }

    #[test]
    fn from_urn_builds_a_code_item_without_a_scheme_or_version() {
        let code = CodeItem::from_urn("urn:oid:1.2.3", "Example meaning");
        assert_eq!(code.value, "urn:oid:1.2.3");
        assert_eq!(code.scheme, "");
        assert_eq!(code.meaning.as_deref(), Some("Example meaning"));
        assert_eq!(code.scheme_version, None);

        let mut ds = Dataset::new();
        code.write_into(&mut ds);
        assert_eq!(
            ds.try_get_str(URN_CODE_VALUE).as_deref(),
            Some("urn:oid:1.2.3")
        );
        assert_eq!(
            ds.try_get_str(CODE_MEANING).as_deref(),
            Some("Example meaning")
        );
        assert!(ds.try_get_str(CODING_SCHEME_DESIGNATOR).is_none());
        assert!(ds.try_get_str(CODING_SCHEME_VERSION).is_none());
    }

    #[test]
    fn equality_includes_the_meaning_text() {
        // deliberately stricter than fo-dicom's value+scheme+version comparison
        let finding = CodeItem::new("121071", "DCM").with_meaning("Finding");
        let observation = CodeItem::new("121071", "DCM").with_meaning("Observation");
        assert_ne!(
            finding, observation,
            "the meaning text participates in equality"
        );
        assert_eq!(
            finding,
            CodeItem::new("121071", "DCM").with_meaning("Finding")
        );
        assert_ne!(
            finding,
            CodeItem::new("121071", "DCM"),
            "no meaning differs from a meaning"
        );
    }

    #[test]
    fn reading_a_dataset_without_a_code_is_an_error() {
        let ds = Dataset::new();
        assert!(CodeItem::read_from(&ds).is_err());
    }

    #[test]
    fn a_code_wraps_into_a_sequence_item() {
        let code = CodeItem::new("T-A0100", "SRT");
        let holder = code.to_sequence(Tag::CONCEPT_NAME_CODE_SEQUENCE);
        let sequence = holder
            .try_get_sequence(Tag::CONCEPT_NAME_CODE_SEQUENCE)
            .unwrap();
        assert_eq!(sequence.items().len(), 1);
        assert_eq!(CodeItem::read_from(&sequence.items()[0]).unwrap(), code);
    }

    #[test]
    fn http_values_use_urn_code_value_like_fo_dicom() {
        for value in [
            "http://example.org/codes/1",
            "HTTPS://example.org/codes/1",
            "urn:oid:1.2.3",
        ] {
            let mut ds = Dataset::new();
            CodeItem::new(value, "DCM").write_into(&mut ds);
            assert_eq!(ds.try_get_str(URN_CODE_VALUE).as_deref(), Some(value));
            assert_eq!(vr_of(&ds, URN_CODE_VALUE), Some(Vr::Ur));
            assert!(
                !ds.contains(&CODE_VALUE),
                "{value} must not be written as a short CodeValue"
            );
            assert_eq!(CodeItem::read_from(&ds).unwrap().value, value);
        }
        // a short non-URN stays in CodeValue
        let mut ds = Dataset::new();
        CodeItem::new("ABC", "DCM").write_into(&mut ds);
        assert_eq!(vr_of(&ds, CODE_VALUE), Some(Vr::Sh));
    }
}
