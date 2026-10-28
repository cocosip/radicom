//! Transform rule DSL for dataset mutation (XFORM-01; docs/02-development-guide.md §2).
//!
//! Ports fo-dicom `DicomTransformRules`: a [`TransformRule`] rewrites one element
//! (set/map/copy/regex/prefix/append/trim/pad/truncate/upper/lower/generate UID/
//! remove). Rules run from a [`TransformRuleSet`] with optional [`MatchRuleSet`]
//! conditions, and can record each element's original value into a Modified
//! Attributes Sequence item before changing it.

use crate::model::dataset::Dataset;
use crate::model::item::DicomItem;
use crate::model::masked_tag::MaskedTag;
use crate::model::tag::Tag;
use crate::model::uid::Uid;
use crate::rules::match_rule::MatchRuleSet;

/// Which end(s) to trim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimPosition {
    Start,
    End,
    Both,
}

/// A single dataset transformation (fo-dicom `IDicomTransformRule`).
#[derive(Debug, Clone)]
pub enum TransformRule {
    /// Remove every element matching a masked tag (e.g. `(60xx,3000)`).
    RemoveElement(MaskedTag),
    /// Set `(tag)` to a string.
    SetValue(Tag, String),
    /// If `(tag)` equals `match`, set it to `value`.
    MapValue(Tag, String, String),
    /// Copy the value of `src` to `dst`.
    CopyValue(Tag, Tag),
    /// Regex replace on `(tag)` (`anonymize` feature).
    #[cfg(feature = "anonymize")]
    Regex(Tag, String, String),
    /// Prepend a string to `(tag)`.
    Prefix(Tag, String),
    /// Append a string to `(tag)`.
    Append(Tag, String),
    /// Repeatedly trim a fixed substring from the given end(s).
    TrimString(Tag, TrimPosition, String),
    /// Trim whitespace (or a set of characters) from the given end(s).
    TrimCharacters(Tag, TrimPosition, Option<Vec<char>>),
    /// Pad to a total length (negative ⇒ left, positive ⇒ right) with `pad`.
    PadString(Tag, i32, char),
    /// Truncate each backslash-separated component to `len` characters.
    Truncate(Tag, usize),
    /// Split on any of `separators` and re-join with a format string where `{0}`
    /// is replaced by the joined components (`format` is a plain template).
    SplitFormat(Tag, Vec<char>, String),
    ToUpper(Tag),
    ToLower(Tag),
    /// Generate a fresh UID for `(tag)`.
    GenerateUid(Tag),
}

impl TransformRule {
    /// Applies the rule to `dataset`.
    pub fn apply(&self, dataset: &mut Dataset) {
        self.apply_impl(dataset, None);
    }

    /// Applies the rule, recording the original value of each touched element into
    /// `modified` (a Modified Attributes Sequence item) before it changes.
    pub fn apply_recording(&self, dataset: &mut Dataset, modified: &mut Dataset) {
        self.apply_impl(dataset, Some(modified));
    }

    fn apply_impl(&self, dataset: &mut Dataset, mut modified: Option<&mut Dataset>) {
        match self {
            TransformRule::RemoveElement(mask) => {
                let victims: Vec<Tag> = dataset
                    .items()
                    .filter(|item: &&DicomItem| mask.matches_u32(item.tag().to_u32()))
                    .map(|item| item.tag().clone())
                    .collect();
                for tag in victims {
                    record(&mut modified, dataset, tag.clone());
                    dataset.remove(&tag);
                }
            }
            TransformRule::SetValue(tag, value) => {
                record(&mut modified, dataset, tag.clone());
                dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), value.clone());
            }
            TransformRule::MapValue(tag, from, to) => {
                if dataset.try_get_str(tag).as_deref() == Some(from.as_str()) {
                    record(&mut modified, dataset, tag.clone());
                    dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), to.clone());
                }
            }
            TransformRule::CopyValue(src, dst) => {
                let found = dataset.items().find(|i| i.tag() == src).cloned();
                if let Some(item) = found {
                    record(&mut modified, dataset, dst.clone());
                    if let DicomItem::Element(e) = &item {
                        let (vr, buffer) = (e.vr(), e.encoded());
                        dataset.insert_or_update(DicomItem::Element(
                            crate::model::element::DicomElement::new_bytes(dst.clone(), vr, buffer),
                        ));
                    }
                }
            }
            #[cfg(feature = "anonymize")]
            TransformRule::Regex(tag, pattern, replacement) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    if let Ok(re) = regex::Regex::new(pattern) {
                        record(&mut modified, dataset, tag.clone());
                        let replaced = re.replace_all(&value, replacement.as_str()).into_owned();
                        dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), replaced);
                    }
                }
            }
            TransformRule::Prefix(tag, prefix) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag.clone());
                    dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), format!("{prefix}{value}"));
                }
            }
            TransformRule::Append(tag, suffix) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag.clone());
                    dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), format!("{value}{suffix}"));
                }
            }
            TransformRule::TrimString(tag, position, trim) => {
                if let Some(mut value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag.clone());
                    if matches!(position, TrimPosition::Start | TrimPosition::Both) && !trim.is_empty() {
                        while value.starts_with(trim.as_str()) {
                            value = value[trim.len()..].to_string();
                        }
                    }
                    if matches!(position, TrimPosition::End | TrimPosition::Both) && !trim.is_empty() {
                        while value.ends_with(trim.as_str()) {
                            value.truncate(value.len() - trim.len());
                        }
                    }
                    dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), value);
                }
            }
            TransformRule::TrimCharacters(tag, position, chars) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag.clone());
                    let trimmed = match chars {
                        Some(set) => match position {
                            TrimPosition::Both => value.trim_matches(|c| set.contains(&c)),
                            TrimPosition::Start => value.trim_start_matches(|c| set.contains(&c)),
                            TrimPosition::End => value.trim_end_matches(|c| set.contains(&c)),
                        },
                        None => match position {
                            TrimPosition::Both => value.trim(),
                            TrimPosition::Start => value.trim_start(),
                            TrimPosition::End => value.trim_end(),
                        },
                    }
                    .to_string();
                    dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), trimmed);
                }
            }
            TransformRule::PadString(tag, total_length, pad) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag.clone());
                    let padded = if *total_length < 0 {
                        let width = (-*total_length) as usize;
                        format!("{value:>width$}", width = width).replace(' ', &pad.to_string())
                    } else {
                        let width = *total_length as usize;
                        format!("{value:<width$}", width = width).replace(' ', &pad.to_string())
                    };
                    dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), padded);
                }
            }
            TransformRule::Truncate(tag, length) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag.clone());
                    let truncated = value
                        .split('\\')
                        .map(|part| part.chars().take(*length).collect::<String>())
                        .collect::<Vec<_>>()
                        .join("\\");
                    dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), truncated);
                }
            }
            TransformRule::SplitFormat(tag, separators, format) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag.clone());
                    let parts: Vec<&str> = value.split(|c| separators.contains(&c)).collect();
                    let joined = parts.join("/");
                    let formatted = format.replace("{0}", &joined);
                    dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), formatted);
                }
            }
            TransformRule::ToUpper(tag) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag.clone());
                    dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), value.to_uppercase());
                }
            }
            TransformRule::ToLower(tag) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag.clone());
                    dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), value.to_lowercase());
                }
            }
            TransformRule::GenerateUid(tag) => {
                if dataset.contains(tag) {
                    record(&mut modified, dataset, tag.clone());
                    dataset.insert_str(tag.clone(), tag_vr_or(dataset, tag.clone()), Uid::generate_value());
                }
            }
        }
    }
}

/// A set of transform rules with optional match conditions (fo-dicom
/// `DicomTransformRuleSet`).
#[derive(Debug, Clone, Default)]
pub struct TransformRuleSet {
    pub conditions: Option<MatchRuleSet>,
    pub rules: Vec<TransformRule>,
}

impl TransformRuleSet {
    pub fn new() -> Self {
        TransformRuleSet::default()
    }

    pub fn with_condition(mut self, conditions: MatchRuleSet) -> Self {
        self.conditions = Some(conditions);
        self
    }

    pub fn with(mut self, rule: TransformRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Applies every rule, but only when the conditions (if any) match.
    pub fn apply(&self, dataset: &mut Dataset) {
        if let Some(conditions) = &self.conditions {
            if !conditions.matches(dataset) {
                return;
            }
        }
        for rule in &self.rules {
            rule.apply(dataset);
        }
    }

    /// Like [`apply`](Self::apply) but records original values into `modified`.
    pub fn apply_recording(&self, dataset: &mut Dataset, modified: &mut Dataset) {
        if let Some(conditions) = &self.conditions {
            if !conditions.matches(dataset) {
                return;
            }
        }
        for rule in &self.rules {
            rule.apply_recording(dataset, modified);
        }
    }
}

/// Clones the current `(tag)` element into `modified`, when both exist.
fn record(modified: &mut Option<&mut Dataset>, dataset: &Dataset, tag: Tag) {
    let Some(target) = modified.as_deref_mut() else {
        return;
    };
    if let Some(item) = dataset.items().find(|i| i.tag() == &tag).cloned() {
        target.insert_or_update(item);
    }
}

/// The dataset's own VR for the tag when present, else the dictionary VR, else LO.
fn tag_vr_or(dataset: &Dataset, tag: Tag) -> crate::model::vr::Vr {
    if let Ok(element) = dataset.get_element(tag) {
        return element.vr();
    }
    crate::model::vr::Vr::Lo
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::vr::Vr;

    fn dataset() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds.insert_str(Tag::MODALITY, Vr::Cs, "ct");
        ds.insert_str(Tag::OTHER_PATIENT_I_DS, Vr::Sh, "ACC123");
        ds
    }

    fn value(ds: &Dataset, group: u16, element: u16) -> Option<String> {
        ds.try_get_str(Tag::new(group, element))
    }

    #[test]
    fn set_and_map_value() {
        let mut ds = dataset();
        TransformRule::SetValue(Tag::MODALITY, "MR".into()).apply(&mut ds);
        assert_eq!(value(&ds, 0x0008, 0x0060).as_deref(), Some("MR"));

        let mut ds = dataset();
        TransformRule::MapValue(Tag::MODALITY, "ct".into(), "CT".into()).apply(&mut ds);
        assert_eq!(value(&ds, 0x0008, 0x0060).as_deref(), Some("CT"));
        // a non-matching map leaves it alone
        let mut ds = dataset();
        TransformRule::MapValue(Tag::MODALITY, "zz".into(), "XX".into()).apply(&mut ds);
        assert_eq!(value(&ds, 0x0008, 0x0060).as_deref(), Some("ct"));
    }

    #[test]
    fn copy_value() {
        let mut ds = dataset();
        TransformRule::CopyValue(Tag::OTHER_PATIENT_I_DS, Tag::PATIENT_ID).apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x0020).as_deref(), Some("ACC123"));
    }

    #[test]
    fn prefix_append_and_case() {
        let mut ds = dataset();
        TransformRule::Prefix(Tag::OTHER_PATIENT_I_DS, "P-".into()).apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x1000).as_deref(), Some("P-ACC123"));

        let mut ds = dataset();
        TransformRule::Append(Tag::OTHER_PATIENT_I_DS, "-X".into()).apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x1000).as_deref(), Some("ACC123-X"));

        let mut ds = dataset();
        TransformRule::ToUpper(Tag::MODALITY).apply(&mut ds);
        assert_eq!(value(&ds, 0x0008, 0x0060).as_deref(), Some("CT"));
    }

    #[test]
    fn trim_and_pad_and_truncate() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "xxHelloxx");
        TransformRule::TrimString(Tag::PATIENT_NAME, TrimPosition::Both, "xx".into()).apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x0010).as_deref(), Some("Hello"));

        let mut ds = Dataset::new();
        ds.insert_str(Tag::OTHER_PATIENT_I_DS, Vr::Sh, "7");
        TransformRule::PadString(Tag::OTHER_PATIENT_I_DS, 3, '0').apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x1000).as_deref(), Some("700"));

        let mut ds = Dataset::new();
        ds.insert_str(Tag::OTHER_PATIENT_I_DS, Vr::Sh, "ABCDEF\\GHIJKL");
        TransformRule::Truncate(Tag::OTHER_PATIENT_I_DS, 3).apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x1000).as_deref(), Some("ABC\\GHI"));
    }

    #[test]
    fn generate_uid_replaces_the_value() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3");
        TransformRule::GenerateUid(Tag::SOP_INSTANCE_UID).apply(&mut ds);
        let new_uid = value(&ds, 0x0008, 0x0018).unwrap();
        assert_ne!(new_uid, "1.2.3");
        assert!(Uid::is_valid(&new_uid));
    }

    #[test]
    fn remove_element_by_masked_tag() {
        let mut ds = Dataset::new();
        let ow = |tag: Tag| {
            crate::model::item::DicomItem::Element(crate::model::element::DicomElement::new_bytes(
                tag,
                Vr::Ow,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vec![1u8, 2, 3, 4])),
            ))
        };
        ds.insert_or_update(ow(Tag::new(0x6000, 0x3000)));
        ds.insert_or_update(ow(Tag::new(0x6002, 0x3000)));
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        let mask = MaskedTag::parse("(60xx,3000)").unwrap();
        TransformRule::RemoveElement(mask).apply(&mut ds);
        assert!(!ds.contains(&Tag::new(0x6000, 0x3000)));
        assert!(!ds.contains(&Tag::new(0x6002, 0x3000)));
        assert!(ds.contains(&Tag::PATIENT_NAME), "non-matching tag survives");
    }

    #[test]
    fn recording_original_values_into_modified_attributes() {
        let mut ds = dataset();
        let mut modified = Dataset::new();
        TransformRule::ToUpper(Tag::MODALITY).apply_recording(&mut ds, &mut modified);
        // the dataset is transformed, the original is preserved in `modified`
        assert_eq!(value(&ds, 0x0008, 0x0060).as_deref(), Some("CT"));
        assert_eq!(value(&modified, 0x0008, 0x0060).as_deref(), Some("ct"));
    }

    #[test]
    fn a_condition_set_gates_the_transform() {
        use crate::rules::match_rule::{MatchOperator, MatchRule, MatchRuleSet};
        let conditions = MatchRuleSet::new(MatchOperator::And)
            .with(MatchRule::Equals(Tag::MODALITY, "ct".into()));
        let set = TransformRuleSet::new()
            .with_condition(conditions)
            .with(TransformRule::ToUpper(Tag::MODALITY));

        let mut ds = dataset();
        set.apply(&mut ds);
        assert_eq!(value(&ds, 0x0008, 0x0060).as_deref(), Some("CT"), "condition matched, rule ran");

        // a non-matching condition leaves the dataset untouched
        let mut other = Dataset::new();
        other.insert_str(Tag::MODALITY, Vr::Cs, "MR");
        set.apply(&mut other);
        assert_eq!(value(&other, 0x0008, 0x0060).as_deref(), Some("MR"));
    }

    #[cfg(feature = "anonymize")]
    #[test]
    fn regex_transform_replaces_and_records() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::OTHER_PATIENT_I_DS, Vr::Sh, "ACC-123-XYZ");
        let mut modified = Dataset::new();
        TransformRule::Regex(Tag::OTHER_PATIENT_I_DS, "\\d+".into(), "###".into())
            .apply_recording(&mut ds, &mut modified);
        assert_eq!(value(&ds, 0x0010, 0x1000).as_deref(), Some("ACC-###-XYZ"));
        assert_eq!(value(&modified, 0x0010, 0x1000).as_deref(), Some("ACC-123-XYZ"), "original recorded");
    }
}
