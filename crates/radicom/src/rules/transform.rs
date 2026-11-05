//! Transform rule DSL for dataset mutation.
//!
//! Ports fo-dicom `DicomTransformRules`: a [`TransformRule`] rewrites one element
//! (set/map/copy/regex/prefix/append/trim/pad/truncate/upper/lower/generate UID/
//! remove). Rules run from a [`TransformRuleSet`] with optional [`MatchRuleSet`]
//! conditions, and can record each element's original value into a Modified
//! Attributes Sequence item before changing it.

use crate::model::dataset::Dataset;
use crate::model::element::ElementValue;
use crate::model::item::DicomItem;
use crate::model::masked_tag::MaskedTag;
use crate::model::tag::Tag;
use crate::model::uid::Uid;
use crate::model::vr::Vr;
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
    /// Regex replace on `(tag)` (`regex-rules` feature).
    #[cfg(feature = "regex-rules")]
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
                    record(&mut modified, dataset, &tag);
                    dataset.remove(&tag);
                }
            }
            TransformRule::SetValue(tag, value) => {
                record(&mut modified, dataset, tag);
                let vr = tag_vr_or(dataset, tag.clone());
                insert_text_value(dataset, tag.clone(), vr, value.clone());
            }
            TransformRule::MapValue(tag, from, to) => {
                if dataset.try_get_str(tag).as_deref() == Some(from.as_str()) {
                    record(&mut modified, dataset, tag);
                    let vr = tag_vr_or(dataset, tag.clone());
                    insert_text_value(dataset, tag.clone(), vr, to.clone());
                }
            }
            TransformRule::CopyValue(src, dst) => {
                let found = dataset.items().find(|i| i.tag() == src).cloned();
                if let Some(DicomItem::Element(e)) = found {
                    record(&mut modified, dataset, dst);
                    let dst_vr = tag_vr_or(dataset, dst.clone());
                    match e.value() {
                        // IO-B-05: rebuild under the *destination* VR instead of
                        // carrying the source VR (which left an SH value tagged
                        // as LO, etc.).
                        ElementValue::Str(text) => {
                            insert_text_value(dataset, dst.clone(), dst_vr, text.clone());
                        }
                        ElementValue::Bytes(buffer) if dst_vr.is_string() => {
                            // IO-01: a byte-backed string value keeps its text
                            // through a write→reparse round-trip.
                            match buffer.data() {
                                Ok(data) => {
                                    let text = String::from_utf8_lossy(&data).into_owned();
                                    insert_text_value(dataset, dst.clone(), dst_vr, text);
                                }
                                Err(error) => {
                                    tracing::warn!(?dst, %error, "copy source unreadable; skipped")
                                }
                            }
                        }
                        ElementValue::Bytes(buffer) => {
                            dataset.insert_or_update(DicomItem::Element(
                                crate::model::element::DicomElement::new_bytes(
                                    dst.clone(),
                                    dst_vr,
                                    buffer.clone(),
                                ),
                            ));
                        }
                    }
                }
            }
            #[cfg(feature = "regex-rules")]
            TransformRule::Regex(tag, pattern, replacement) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    if let Ok(re) = regex::Regex::new(pattern) {
                        record(&mut modified, dataset, tag);
                        let replaced = re.replace_all(&value, replacement.as_str()).into_owned();
                        let vr = tag_vr_or(dataset, tag.clone());
                        insert_text_value(dataset, tag.clone(), vr, replaced);
                    }
                }
            }
            TransformRule::Prefix(tag, prefix) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag);
                    let vr = tag_vr_or(dataset, tag.clone());
                    insert_text_value(dataset, tag.clone(), vr, format!("{prefix}{value}"));
                }
            }
            TransformRule::Append(tag, suffix) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag);
                    let vr = tag_vr_or(dataset, tag.clone());
                    insert_text_value(dataset, tag.clone(), vr, format!("{value}{suffix}"));
                }
            }
            TransformRule::TrimString(tag, position, trim) => {
                if let Some(mut value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag);
                    if matches!(position, TrimPosition::Start | TrimPosition::Both)
                        && !trim.is_empty()
                    {
                        while value.starts_with(trim.as_str()) {
                            value = value[trim.len()..].to_string();
                        }
                    }
                    if matches!(position, TrimPosition::End | TrimPosition::Both)
                        && !trim.is_empty()
                    {
                        while value.ends_with(trim.as_str()) {
                            value.truncate(value.len() - trim.len());
                        }
                    }
                    let vr = tag_vr_or(dataset, tag.clone());
                    insert_text_value(dataset, tag.clone(), vr, value);
                }
            }
            TransformRule::TrimCharacters(tag, position, chars) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag);
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
                    let vr = tag_vr_or(dataset, tag.clone());
                    insert_text_value(dataset, tag.clone(), vr, trimmed);
                }
            }
            TransformRule::PadString(tag, total_length, pad) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag);
                    let padded = pad_to_width(&value, *total_length, *pad);
                    let vr = tag_vr_or(dataset, tag.clone());
                    insert_text_value(dataset, tag.clone(), vr, padded);
                }
            }
            TransformRule::Truncate(tag, length) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag);
                    let truncated = value
                        .split('\\')
                        .map(|part| part.chars().take(*length).collect::<String>())
                        .collect::<Vec<_>>()
                        .join("\\");
                    let vr = tag_vr_or(dataset, tag.clone());
                    insert_text_value(dataset, tag.clone(), vr, truncated);
                }
            }
            TransformRule::SplitFormat(tag, separators, format) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag);
                    let parts: Vec<&str> = value.split(|c| separators.contains(&c)).collect();
                    let formatted = format_parts(format, &parts);
                    let vr = tag_vr_or(dataset, tag.clone());
                    insert_text_value(dataset, tag.clone(), vr, formatted);
                }
            }
            TransformRule::ToUpper(tag) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag);
                    let vr = tag_vr_or(dataset, tag.clone());
                    insert_text_value(dataset, tag.clone(), vr, value.to_uppercase());
                }
            }
            TransformRule::ToLower(tag) => {
                if let Some(value) = dataset.try_get_str(tag) {
                    record(&mut modified, dataset, tag);
                    let vr = tag_vr_or(dataset, tag.clone());
                    insert_text_value(dataset, tag.clone(), vr, value.to_lowercase());
                }
            }
            TransformRule::GenerateUid(tag) => {
                if dataset.contains(tag) {
                    record(&mut modified, dataset, tag);
                    let vr = tag_vr_or(dataset, tag.clone());
                    insert_text_value(dataset, tag.clone(), vr, Uid::generate_value());
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
fn record(modified: &mut Option<&mut Dataset>, dataset: &Dataset, tag: &Tag) {
    let Some(target) = modified.as_deref_mut() else {
        return;
    };
    if let Some(item) = dataset.items().find(|i| i.tag() == tag).cloned() {
        target.insert_or_update(item);
    }
}

/// Pads `value` to `total_length` characters with `pad`.
///
/// fo-dicom `DicomTransformRules.PadStringDicomTransformRule`: a negative
/// `total_length` pads on the left, a positive one on the right, and a value
/// that is already long enough is returned unchanged. Unlike `format!` +
/// `replace`, this never touches characters inside the value (FEAT-08).
fn pad_to_width(value: &str, total_length: i32, pad: char) -> String {
    let width = total_length.unsigned_abs() as usize;
    let len = value.chars().count();
    if len >= width {
        return value.to_string();
    }
    let padding = width - len;
    let mut out = String::with_capacity(value.len() + padding * pad.len_utf8());
    if total_length < 0 {
        for _ in 0..padding {
            out.push(pad);
        }
        out.push_str(value);
    } else {
        out.push_str(value);
        for _ in 0..padding {
            out.push(pad);
        }
    }
    out
}

/// Substitutes positional `{0}`, `{1}`, … parts into `format`, like fo-dicom's
/// `string.Format(_format, parts)` (FEAT-09). `{{`/`}}` produce literal braces;
/// an out-of-range index keeps the placeholder text.
fn format_parts(format: &str, parts: &[&str]) -> String {
    let mut out = String::with_capacity(format.len());
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut digits = String::new();
                while let Some(d) = chars.peek().copied().filter(char::is_ascii_digit) {
                    digits.push(d);
                    chars.next();
                }
                if !digits.is_empty() && chars.peek() == Some(&'}') {
                    chars.next();
                    let index = digits.parse::<usize>().ok();
                    match index.and_then(|i| parts.get(i)) {
                        Some(part) => out.push_str(part),
                        None => {
                            out.push('{');
                            out.push_str(&digits);
                            out.push('}');
                        }
                    }
                } else {
                    out.push('{');
                    out.push_str(&digits);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Writes a text value into `tag` according to its VR.
///
/// String VRs store the text directly. Numeric VRs parse the text into typed
/// values (a numeric tag such as Rows=US must never reach `insert_str`, whose
/// `new_str` debug-asserts on a non-string VR). Anything else cannot be
/// represented from text and is skipped with a warning instead of panicking.
fn insert_text_value(dataset: &mut Dataset, tag: Tag, vr: Vr, text: String) {
    if vr.is_string() || vr == Vr::Implicit {
        dataset.insert_str(tag, vr, text);
        return;
    }
    match vr {
        Vr::Us => insert_parsed::<u16>(dataset, tag, vr, &text),
        Vr::Ss => insert_parsed::<i16>(dataset, tag, vr, &text),
        Vr::Ul => insert_parsed::<u32>(dataset, tag, vr, &text),
        Vr::Sl => insert_parsed::<i32>(dataset, tag, vr, &text),
        Vr::Uv => insert_parsed::<u64>(dataset, tag, vr, &text),
        Vr::Sv => insert_parsed::<i64>(dataset, tag, vr, &text),
        Vr::Fl => insert_parsed::<f32>(dataset, tag, vr, &text),
        Vr::Fd => insert_parsed::<f64>(dataset, tag, vr, &text),
        _ => tracing::warn!(
            ?tag,
            %vr,
            "text value cannot be represented as this VR; transform skipped"
        ),
    }
}

fn insert_parsed<T>(dataset: &mut Dataset, tag: Tag, vr: Vr, text: &str)
where
    T: std::str::FromStr,
    Vec<T>: crate::model::element::IntoElement,
{
    match text
        .split('\\')
        .map(|part| part.trim().parse::<T>())
        .collect::<Result<Vec<T>, _>>()
    {
        Ok(values) if !values.is_empty() => {
            dataset.insert_value_with_vr(tag, vr, values);
        }
        _ => tracing::warn!(?tag, %vr, "text value does not parse; transform skipped"),
    }
}

/// The dataset's own VR for the tag when present, else the dictionary VR, else LO.
fn tag_vr_or(dataset: &Dataset, tag: Tag) -> crate::model::vr::Vr {
    if let Ok(element) = dataset.get_element(&tag) {
        return element.vr();
    }
    crate::model::dictionary::dictionary()
        .entry(&tag)
        .vrs
        .first()
        .copied()
        .unwrap_or(crate::model::vr::Vr::Lo)
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
    fn copy_value_uses_the_destination_vr() {
        // IO-B-05: the copied element must take the destination's VR, not the
        // source SH VR.
        let mut ds = dataset();
        TransformRule::CopyValue(Tag::OTHER_PATIENT_I_DS, Tag::PATIENT_ID).apply(&mut ds);
        let element = ds.get_element(Tag::PATIENT_ID).expect("copied");
        assert_eq!(element.vr(), Vr::Lo);
        assert_eq!(element.string_value(), "ACC123");
    }

    #[test]
    fn set_value_on_a_numeric_vr_does_not_panic_and_round_trips() {
        // IO-B-04: SetValue(Rows=US, "512") used to panic inside `new_str`'s
        // debug assertion (or save as a bogus value in release).
        use crate::io::dataset_builder::parse_dataset_bytes;
        use crate::io::writer::{DicomWriter, WriteOptions};
        use crate::model::transfer_syntax::syntaxes;
        let mut ds = Dataset::new();
        TransformRule::SetValue(Tag::ROWS, "512".into()).apply(&mut ds);
        assert_eq!(ds.get::<u16>(Tag::ROWS).unwrap(), 512);
        let mut out = Vec::new();
        DicomWriter::new(WriteOptions::default())
            .write_dataset(&ds, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN, &mut out)
            .expect("encode");
        let back = parse_dataset_bytes(&out, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).expect("decode");
        assert_eq!(back.get::<u16>(Tag::ROWS).unwrap(), 512);
    }

    #[test]
    fn copied_string_value_survives_serialization() {
        // IO-01: CopyValue must preserve a string value through write→reparse,
        // not just in memory (the old byte-backed rebuild serialised as empty).
        use crate::io::dataset_builder::parse_dataset_bytes;
        use crate::io::writer::{DicomWriter, WriteOptions};
        use crate::model::transfer_syntax::syntaxes;
        let mut ds = dataset();
        TransformRule::CopyValue(Tag::OTHER_PATIENT_I_DS, Tag::PATIENT_ID).apply(&mut ds);
        let mut out = Vec::new();
        DicomWriter::new(WriteOptions::default())
            .write_dataset(&ds, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN, &mut out)
            .expect("encode");
        let back = parse_dataset_bytes(&out, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).expect("decode");
        assert_eq!(back.get_str(Tag::PATIENT_ID).unwrap(), "ACC123");
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
        TransformRule::TrimString(Tag::PATIENT_NAME, TrimPosition::Both, "xx".into())
            .apply(&mut ds);
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
    fn trim_split_case_and_pad_variants() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "xxHelloxx");
        TransformRule::TrimCharacters(Tag::PATIENT_NAME, TrimPosition::Start, Some(vec!['x']))
            .apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x0010).as_deref(), Some("Helloxx"));
        TransformRule::TrimCharacters(Tag::PATIENT_NAME, TrimPosition::End, Some(vec!['x']))
            .apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x0010).as_deref(), Some("Hello"));

        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "  Doe  ");
        TransformRule::TrimCharacters(Tag::PATIENT_NAME, TrimPosition::Both, None).apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x0010).as_deref(), Some("Doe"));

        let mut ds = Dataset::new();
        ds.insert_str(Tag::OTHER_PATIENT_I_DS, Vr::Sh, "a,b;c");
        TransformRule::SplitFormat(Tag::OTHER_PATIENT_I_DS, vec![',', ';'], "{0}|{1}".into())
            .apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x1000).as_deref(), Some("a|b"));

        let mut ds = Dataset::new();
        ds.insert_str(Tag::OTHER_PATIENT_I_DS, Vr::Sh, "AbC");
        TransformRule::ToLower(Tag::OTHER_PATIENT_I_DS).apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x1000).as_deref(), Some("abc"));
        TransformRule::PadString(Tag::OTHER_PATIENT_I_DS, -5, '0').apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x1000).as_deref(), Some("00abc"));
    }

    #[test]
    fn pad_string_only_inserts_the_pad_character() {
        // FEAT-08: internal spaces must survive padding (and no padding means no
        // change at all).
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe John");
        TransformRule::PadString(Tag::PATIENT_NAME, 12, '0').apply(&mut ds);
        assert_eq!(
            value(&ds, 0x0010, 0x0010).as_deref(),
            Some("Doe John0000"),
            "spaces inside the value are not pad targets"
        );

        TransformRule::PadString(Tag::PATIENT_NAME, 3, '0').apply(&mut ds);
        assert_eq!(
            value(&ds, 0x0010, 0x0010).as_deref(),
            Some("Doe John0000"),
            "a value already wider than the target is unchanged"
        );
    }

    #[test]
    fn split_format_uses_positional_parts_and_brace_escapes() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::OTHER_PATIENT_I_DS, Vr::Sh, "a^b^c");
        TransformRule::SplitFormat(
            Tag::OTHER_PATIENT_I_DS,
            vec!['^'],
            "{2}-{0} ({1}) {{x}}".into(),
        )
        .apply(&mut ds);
        assert_eq!(
            value(&ds, 0x0010, 0x1000).as_deref(),
            Some("c-a (b) {x}"),
            "each placeholder addresses its part and {{}} is a literal brace"
        );

        // an out-of-range index is preserved instead of panicking
        TransformRule::SplitFormat(Tag::OTHER_PATIENT_I_DS, vec![' '], "{9}".into()).apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x1000).as_deref(), Some("{9}"));
    }

    #[test]
    fn set_value_on_an_absent_tag_uses_the_dictionary_vr() {
        // FEAT-10: the dictionary VR (DA here) is authoritative for a tag not
        // yet present, not LO.
        let mut ds = Dataset::new();
        TransformRule::SetValue(Tag::STUDY_DATE, "20260101".into()).apply(&mut ds);
        let element = ds.get_element(Tag::STUDY_DATE).expect("value inserted");
        assert_eq!(element.vr(), Vr::Da);
    }

    #[test]
    fn absent_tags_and_generate_uid_are_no_ops() {
        let mut ds = Dataset::new();
        TransformRule::Prefix(Tag::MODALITY, "P-".into()).apply(&mut ds);
        TransformRule::Append(Tag::MODALITY, "-X".into()).apply(&mut ds);
        TransformRule::TrimString(Tag::MODALITY, TrimPosition::Both, "x".into()).apply(&mut ds);
        TransformRule::TrimCharacters(Tag::MODALITY, TrimPosition::Both, None).apply(&mut ds);
        TransformRule::PadString(Tag::MODALITY, 4, '0').apply(&mut ds);
        TransformRule::Truncate(Tag::MODALITY, 2).apply(&mut ds);
        TransformRule::SplitFormat(Tag::MODALITY, vec![','], "{0}".into()).apply(&mut ds);
        TransformRule::ToUpper(Tag::MODALITY).apply(&mut ds);
        TransformRule::ToLower(Tag::MODALITY).apply(&mut ds);
        TransformRule::GenerateUid(Tag::SOP_INSTANCE_UID).apply(&mut ds);
        TransformRule::CopyValue(Tag::MODALITY, Tag::PATIENT_ID).apply(&mut ds);
        assert!(
            ds.items().next().is_none(),
            "rules on absent tags do nothing"
        );
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
        assert_eq!(
            value(&ds, 0x0008, 0x0060).as_deref(),
            Some("CT"),
            "condition matched, rule ran"
        );

        // a non-matching condition leaves the dataset untouched
        let mut other = Dataset::new();
        other.insert_str(Tag::MODALITY, Vr::Cs, "MR");
        set.apply(&mut other);
        assert_eq!(value(&other, 0x0008, 0x0060).as_deref(), Some("MR"));
    }

    #[cfg(feature = "regex-rules")]
    #[test]
    fn regex_transform_replaces_and_records() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::OTHER_PATIENT_I_DS, Vr::Sh, "ACC-123-XYZ");
        let mut modified = Dataset::new();
        TransformRule::Regex(Tag::OTHER_PATIENT_I_DS, "\\d+".into(), "###".into())
            .apply_recording(&mut ds, &mut modified);
        assert_eq!(value(&ds, 0x0010, 0x1000).as_deref(), Some("ACC-###-XYZ"));
        assert_eq!(
            value(&modified, 0x0010, 0x1000).as_deref(),
            Some("ACC-123-XYZ"),
            "original recorded"
        );
    }

    #[test]
    fn trim_boundaries_and_empty_trim_text() {
        let mut ds = dataset();
        // an empty trim string is a no-op at either end
        TransformRule::TrimString(Tag::PATIENT_NAME, TrimPosition::Start, "".into()).apply(&mut ds);
        TransformRule::TrimString(Tag::PATIENT_NAME, TrimPosition::End, "".into()).apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x0010).as_deref(), Some("Doe^John"));

        // whitespace trimming respects Start/End when no character set is given
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "  Doe^John  ");
        TransformRule::TrimCharacters(Tag::PATIENT_NAME, TrimPosition::Start, None).apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x0010).as_deref(), Some("Doe^John  "));
        TransformRule::TrimCharacters(Tag::PATIENT_NAME, TrimPosition::End, None).apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x0010).as_deref(), Some("Doe^John"));

        // an explicit set trims both ends
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "xxHello");
        TransformRule::TrimCharacters(Tag::PATIENT_NAME, TrimPosition::Both, Some(vec!['x']))
            .apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x0010).as_deref(), Some("Hello"));
    }

    #[test]
    fn split_format_keeps_an_unclosed_brace() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        TransformRule::SplitFormat(Tag::PATIENT_NAME, vec!['^'], "{0".into()).apply(&mut ds);
        assert_eq!(value(&ds, 0x0010, 0x0010).as_deref(), Some("{0"));
    }

    #[test]
    fn copy_value_byte_string_and_binary_destinations() {
        use crate::model::buffer::MemoryBuffer;
        use crate::model::element::DicomElement;

        fn bytes_element(tag: Tag, vr: Vr, bytes: &[u8]) -> DicomItem {
            DicomItem::Element(DicomElement::new_bytes(
                tag,
                vr,
                std::sync::Arc::new(MemoryBuffer::from(bytes.to_vec())),
            ))
        }

        // a byte-backed string VR copied into a string destination keeps its text
        let mut ds = Dataset::new();
        ds.insert_or_update(bytes_element(Tag::OTHER_PATIENT_I_DS, Vr::Sh, b"ACC123"));
        TransformRule::CopyValue(Tag::OTHER_PATIENT_I_DS, Tag::PATIENT_ID).apply(&mut ds);
        assert_eq!(ds.try_get_str(Tag::PATIENT_ID).as_deref(), Some("ACC123"));

        // an unreadable byte-backed string is skipped with a warning, not panicked
        #[derive(Debug)]
        struct FailingBuffer;
        impl crate::model::buffer::ByteBuffer for FailingBuffer {
            fn size(&self) -> u64 {
                3
            }
            fn is_memory(&self) -> bool {
                false
            }
            fn data(&self) -> std::io::Result<std::borrow::Cow<'_, [u8]>> {
                Err(std::io::Error::other("boom"))
            }
        }
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::OTHER_PATIENT_I_DS,
            Vr::Sh,
            std::sync::Arc::new(FailingBuffer),
        )));
        TransformRule::CopyValue(Tag::OTHER_PATIENT_I_DS, Tag::PATIENT_ID).apply(&mut ds);
        assert!(ds.get_element(Tag::PATIENT_ID).is_err());

        // source bytes copied to a binary destination keep the raw bytes
        let mut ds = Dataset::new();
        ds.insert_or_update(bytes_element(
            Tag::new(0x7777, 0x0001),
            Vr::Ow,
            &[1, 2, 3, 4],
        ));
        TransformRule::CopyValue(Tag::new(0x7777, 0x0001), Tag::PIXEL_DATA).apply(&mut ds);
        let copied = ds.get_element(Tag::PIXEL_DATA).expect("binary target");
        assert_eq!(copied.value().as_buffer().unwrap().size(), 4);
    }

    #[test]
    fn set_value_parses_each_numeric_vr_and_skips_binary() {
        use crate::model::buffer::EmptyBuffer;
        use crate::model::element::DicomElement;

        let cases: &[(Vr, &str)] = &[
            (Vr::Ss, "-7"),
            (Vr::Ul, "42"),
            (Vr::Sl, "-42"),
            (Vr::Uv, "42"),
            (Vr::Sv, "-42"),
            (Vr::Fl, "1.5"),
            (Vr::Fd, "2.5"),
        ];
        for (i, (vr, text)) in cases.iter().enumerate() {
            let tag = Tag::new(0x7777, 0x1000 + i as u16);
            let mut ds = Dataset::new();
            ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
                tag.clone(),
                *vr,
                EmptyBuffer::shared(),
            )));
            TransformRule::SetValue(tag.clone(), (*text).to_string()).apply(&mut ds);
            assert!(ds.get_element(tag).is_ok(), "{vr:?} value set");
        }

        // a VR that cannot be represented from text is skipped with a warning
        let tag = Tag::new(0x7777, 0x2000);
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            tag.clone(),
            Vr::Ob,
            EmptyBuffer::shared(),
        )));
        TransformRule::SetValue(tag.clone(), "x".into()).apply(&mut ds);
        assert!(ds.get_element(tag).is_ok());
    }

    #[test]
    fn set_value_on_unparseable_numeric_is_a_no_op() {
        let mut ds = Dataset::new();
        TransformRule::SetValue(Tag::ROWS, "not-a-number".into()).apply(&mut ds);
        assert!(ds.get_element(Tag::ROWS).is_err());
    }

    #[cfg(feature = "regex-rules")]
    #[test]
    fn regex_on_an_absent_tag_is_a_no_op() {
        let mut ds = Dataset::new();
        TransformRule::Regex(Tag::MODALITY, "x".into(), "y".into()).apply(&mut ds);
        assert!(ds.items().next().is_none());
    }

    #[test]
    fn rule_set_without_conditions_applies_and_records() {
        let set = TransformRuleSet::new().with(TransformRule::ToUpper(Tag::MODALITY));

        let mut ds = Dataset::new();
        ds.insert_str(Tag::MODALITY, Vr::Cs, "ct");
        set.apply(&mut ds);
        assert_eq!(ds.try_get_str(Tag::MODALITY).as_deref(), Some("CT"));

        let mut ds = Dataset::new();
        ds.insert_str(Tag::MODALITY, Vr::Cs, "ct");
        let mut modified = Dataset::new();
        set.apply_recording(&mut ds, &mut modified);
        assert_eq!(modified.try_get_str(Tag::MODALITY).as_deref(), Some("ct"));
    }

    #[test]
    fn a_transform_with_an_unmatched_condition_is_a_no_op() {
        use crate::rules::match_rule::{MatchOperator, MatchRule};
        let conditions = MatchRuleSet::new(MatchOperator::And).with(MatchRule::Bool(false));
        let transform = TransformRuleSet::new()
            .with(TransformRule::ToUpper(Tag::MODALITY))
            .with_condition(conditions);
        let mut ds = dataset();
        transform.apply(&mut ds);
        assert_eq!(value(&ds, 0x0008, 0x0060).as_deref(), Some("ct"));

        let mut modified = Dataset::new();
        transform.apply_recording(&mut ds, &mut modified);
        assert_eq!(value(&modified, 0x0008, 0x0060), None);
    }
}
