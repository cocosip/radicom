//! SR support types (SR-02; docs/08-serialization-media.md §4.2).
//!
//! `CodeItem` is the coded-entry triple that SR (and many other IODs) reuse.
//! DICOM splits it across three tag pairs by value length: the "short" form
//! (0018,0090../0100../0102..) for values ≤16 bytes, the "long" form
//! (0008,0100../0102../0104..) otherwise, and a bare URN in the long code-value
//! slot. This module writes the right pair for the value and reads either back.

use crate::error::{DataError, Error};
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::model::vr::Vr;

/// Tag layout for one code triple (§4.2).
pub struct CodeTags {
    pub value: Tag,
    pub scheme: Tag,
    pub meaning: Tag,
    pub scheme_version: Tag,
}

/// Short form: `CodeValue` ≤ 16 bytes (`(0018,0090)` family).
pub static SHORT: CodeTags = CodeTags {
    value: Tag::new(0x0018, 0x0090),
    scheme: Tag::new(0x0018, 0x0100),
    meaning: Tag::new(0x0018, 0x0102),
    scheme_version: Tag::new(0x0018, 0x0104),
};

/// Long form: `CodeValue` > 16 bytes (`(0008,0100)` family).
pub static LONG: CodeTags = CodeTags {
    value: Tag::new(0x0008, 0x0100),
    scheme: Tag::new(0x0008, 0x0102),
    meaning: Tag::new(0x0008, 0x0104),
    scheme_version: Tag::new(0x0008, 0x0106),
};

/// A coded entry: value + coding scheme (+ optional meaning/version) (§4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeItem {
    pub value: String,
    pub scheme: String,
    pub meaning: Option<String>,
    pub scheme_version: Option<String>,
}

impl CodeItem {
    pub fn new(value: impl Into<String>, scheme: impl Into<String>) -> Self {
        CodeItem { value: value.into(), scheme: scheme.into(), meaning: None, scheme_version: None }
    }

    pub fn with_meaning(mut self, meaning: impl Into<String>) -> Self {
        self.meaning = Some(meaning.into());
        self
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.scheme_version = Some(version.into());
        self
    }

    /// Writes this code into `dataset`, choosing the short/long tag form by the
    /// code value length and swapping to a URN form when the value is a URN
    /// (§4.2).
    pub fn write_into(&self, dataset: &mut Dataset) {
        let tags = tags_for(&self.value);
        dataset.insert_str(tags.value.clone(), Vr::Sh, self.value.clone());
        dataset.insert_str(tags.scheme.clone(), Vr::Sh, self.scheme.clone());
        if let Some(meaning) = &self.meaning {
            if !meaning.is_empty() {
                dataset.insert_str(tags.meaning.clone(), Vr::Lo, meaning.clone());
            }
        }
        if let Some(version) = &self.scheme_version {
            if !version.is_empty() {
                dataset.insert_str(tags.scheme_version.clone(), Vr::Sh, version.clone());
            }
        }
    }

    /// Reads a code from the sequence item's first matching tag family.
    pub fn read_from(dataset: &Dataset) -> Result<Self, Error> {
        // try both families; the short one wins when both exist
        for tags in [&SHORT, &LONG] {
            if let Some(value) = dataset.try_get_str(&tags.value) {
                return Ok(CodeItem {
                    value: value.trim().to_string(),
                    scheme: dataset.try_get_str(&tags.scheme).unwrap_or_default().trim().to_string(),
                    meaning: dataset.try_get_str(&tags.meaning).map(|m| m.trim().to_string()),
                    scheme_version: dataset
                        .try_get_str(&tags.scheme_version)
                        .map(|v| v.trim().to_string()),
                });
            }
        }
        Err(Error::Data(DataError::Other { message: "no coded entry found".into() }))
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

/// True when the code value must use the long tag family (§4.2).
///
/// DICOM's threshold is 16 in *bytes*; a value longer than that cannot live in
/// the `SH` short slot. A URN (`urn:` prefix or a scheme-qualified value) also
/// forces the long form.
fn needs_long(value: &str) -> bool {
    value.len() > 16 || value.starts_with("urn:") || value.starts_with("URN:")
}

fn tags_for(value: &str) -> &'static CodeTags {
    if needs_long(value) {
        &LONG
    } else {
        &SHORT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_code_writes_the_short_tag_family() {
        let mut ds = Dataset::new();
        CodeItem::new("T-A0100", "SRT").with_meaning("Chest").write_into(&mut ds);
        assert_eq!(ds.try_get_str(&SHORT.value).as_deref(), Some("T-A0100"));
        assert_eq!(ds.try_get_str(&SHORT.scheme).as_deref(), Some("SRT"));
        assert_eq!(ds.try_get_str(&SHORT.meaning).as_deref(), Some("Chest"));
        assert!(ds.try_get_str(&LONG.value).is_none(), "the long family must stay empty");
    }

    #[test]
    fn a_long_code_writes_the_long_tag_family() {
        let mut ds = Dataset::new();
        // 17 bytes forces the long form
        CodeItem::new("12345678901234567", "99DIN").write_into(&mut ds);
        assert_eq!(ds.try_get_str(&LONG.value).as_deref(), Some("12345678901234567"));
        assert!(ds.try_get_str(&SHORT.value).is_none());
    }

    #[test]
    fn a_urn_forces_the_long_family_even_when_short() {
        let mut ds = Dataset::new();
        CodeItem::new("urn:oid:1.2.3", "DCM").write_into(&mut ds);
        assert_eq!(ds.try_get_str(&LONG.value).as_deref(), Some("urn:oid:1.2.3"));
        assert!(ds.try_get_str(&SHORT.value).is_none());
    }

    #[test]
    fn a_code_round_trips_through_a_dataset() {
        let original = CodeItem::new("T-A0100", "SRT").with_meaning("Chest").with_version("1");
        let mut ds = Dataset::new();
        original.write_into(&mut ds);
        let parsed = CodeItem::read_from(&ds).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn reading_a_dataset_without_a_code_is_an_error() {
        let ds = Dataset::new();
        assert!(CodeItem::read_from(&ds).is_err());
    }

    #[test]
    fn a_code_wraps_into_a_sequence_item() {
        let code = CodeItem::new("T-A0100", "SRT");
        let holder = code.to_sequence(Tag::new(0x0040, 0xA043));
        let sequence = holder.try_get_sequence(Tag::new(0x0040, 0xA043)).unwrap();
        assert_eq!(sequence.items().len(), 1);
        assert_eq!(CodeItem::read_from(&sequence.items()[0]).unwrap(), code);
    }
}
