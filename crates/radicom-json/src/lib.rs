//! radicom-json — DICOM JSON (PS3.18) serialization (JSON-01).
//!
//! The model is PS3.18 Annex F: one object per attribute, keyed by the
//! 8-hex-digit tag, always carrying `"vr"` plus one value carrier
//! (`Value`, `InlineBinary` or `BulkDataURI`).
//!
//! ```json
//! { "00100010": { "vr": "PN", "Value": [{ "Alphabetic": "Doe^John" }] } }
//! ```
//!
//! Fragment sequences are deliberately not representable (§1.1 of
//! docs/08-serialization-media.md): they return an error that suggests
//! transcoding, matching fo-dicom.

use base64::Engine as _;
use radicom::model::buffer::{MemoryBuffer, SharedBuffer};
use radicom::model::dataset::Dataset;
use radicom::model::element::DicomElement;
use radicom::model::item::{DicomItem, DicomSequence};
use radicom::model::person_name::PersonName;
use radicom::model::tag::Tag;
use radicom::model::vr::Vr;
use serde_json::{Map, Number, Value};
use std::sync::Arc;

/// Errors of the JSON model (§1).
#[derive(Debug, thiserror::Error)]
pub enum JsonError {
    #[error("fragment sequences cannot be represented in DICOM JSON; transcode the pixel data to a native syntax first")]
    Fragments,
    #[error("attribute {tag} has no \"vr\" property")]
    MissingVr { tag: String },
    #[error("attribute {tag} has unknown VR \"{vr}\"")]
    UnknownVr { tag: String, vr: String },
    #[error("attribute {tag} has a malformed value: {reason}")]
    Malformed { tag: String, reason: String },
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("dataset error: {0}")]
    Dataset(#[from] radicom::Error),
}

/// Result alias for the JSON crate.
pub type Result<T> = std::result::Result<T, JsonError>;

/// How IS/DS/SV/UV values are written (§1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NumberMode {
    /// Always a JSON number when the value parses, else the string.
    AsNumber,
    /// Always the exact string form.
    AsString,
    /// A number only when writing it back yields the same text; strings keep
    /// trailing zeros and other loss-sensitive forms.
    #[default]
    PreferablyAsNumber,
}

/// Serialization options (§1.2).
#[derive(Debug, Clone)]
pub struct JsonOptions {
    /// Non-standard: use attribute keywords instead of tags as keys. Only
    /// affects writing, and only when the dictionary knows the tag.
    pub tags_as_keywords: bool,
    /// Reserved for the read path; parsing is always tolerant (§1.3).
    pub validate_on_read: bool,
    pub numbers: NumberMode,
    pub format_indented: bool,
}

impl Default for JsonOptions {
    fn default() -> Self {
        JsonOptions {
            tags_as_keywords: false,
            validate_on_read: false,
            numbers: NumberMode::PreferablyAsNumber,
            format_indented: false,
        }
    }
}

/// The 8-character hex key of a tag (§1.1).
pub fn tag_key(tag: &Tag) -> String {
    tag.to_json_string()
}

/// Parses a tag key (8 hex digits); anything else is skipped on read.
fn parse_tag_key(key: &str) -> Option<Tag> {
    Tag::parse(key).ok()
}

/// Serializes a dataset to DICOM JSON (§1.4).
pub fn dataset_to_json(dataset: &Dataset, options: &JsonOptions) -> Result<String> {
    let value = dataset_to_value(dataset, options)?;
    if options.format_indented {
        Ok(serde_json::to_string_pretty(&value)?)
    } else {
        Ok(serde_json::to_string(&value)?)
    }
}

/// Serializes a dataset to a JSON value tree (§1.4).
pub fn dataset_to_value(dataset: &Dataset, options: &JsonOptions) -> Result<Value> {
    let mut object = Map::new();
    for item in dataset.items() {
        match item {
            DicomItem::Element(element) => {
                object.insert(key_for(element.tag(), options), element_to_value(element, options)?);
            }
            DicomItem::Sequence(sequence) => {
                object.insert(key_for(sequence.tag(), options), sequence_to_value(sequence, options)?);
            }
            DicomItem::Fragments(_) => return Err(JsonError::Fragments),
        }
    }
    Ok(Value::Object(object))
}

fn key_for(tag: &Tag, options: &JsonOptions) -> String {
    if options.tags_as_keywords {
        let keyword = radicom::default_dictionary().entry(tag).keyword;
        if !keyword.is_empty() {
            return keyword.to_string();
        }
    }
    tag_key(tag)
}

fn sequence_to_value(sequence: &DicomSequence, options: &JsonOptions) -> Result<Value> {
    let mut values = Vec::with_capacity(sequence.items().len());
    for item in sequence.items() {
        values.push(dataset_to_value(item, options)?);
    }
    let mut object = Map::new();
    object.insert("vr".to_string(), Value::String("SQ".to_string()));
    object.insert("Value".to_string(), Value::Array(values));
    Ok(Value::Object(object))
}

/// True for VRs carried as a base64 `InlineBinary` (§1.1).
fn is_binary(vr: Vr) -> bool {
    matches!(vr, Vr::Ob | Vr::Od | Vr::Of | Vr::Ol | Vr::Ov | Vr::Ow | Vr::Un)
}

fn element_to_value(element: &DicomElement, options: &JsonOptions) -> Result<Value> {
    let vr = element.vr();
    let mut object = Map::new();
    object.insert("vr".to_string(), Value::String(vr.code().to_string()));

    if is_binary(vr) {
        // an empty binary value carries no carrier at all (§1.1)
        if let Some(buffer) = element.value().as_buffer() {
            if buffer.size() > 0 {
                let encoded = base64::engine::general_purpose::STANDARD.encode(buffer.data().as_ref());
                object.insert("InlineBinary".to_string(), Value::String(encoded));
            }
        }
        return Ok(Value::Object(object));
    }

    if vr == Vr::Sq {
        // an SQ stored as an element (a length-0 sequence) still serializes as one
        object.insert("Value".to_string(), Value::Array(Vec::new()));
        return Ok(Value::Object(object));
    }

    let values = element_values(element, vr, options)?;
    // an empty element omits Value entirely (§1.1)
    if !values.is_empty() {
        object.insert("Value".to_string(), Value::Array(values));
    }
    Ok(Value::Object(object))
}

/// Builds the `Value` array for a non-binary element (§1.1).
fn element_values(element: &DicomElement, vr: Vr, options: &JsonOptions) -> Result<Vec<Value>> {
    // bytes that are not a binary VR come from a parsed element (e.g. US);
    // strings are the other possible representation
    if let Some(raw) = element.value().as_buffer() {
        return Ok(raw_value_array(raw, vr, element.tag()));
    }
    let text = element.string_value();
    if text.is_empty() {
        return Ok(Vec::new());
    }

    match vr {
        Vr::Pn => Ok(vec![person_name_value(text)]),
        Vr::At => Ok(text
            .split('\\')
            .filter(|part| !part.is_empty())
            .map(|part| Value::String(part.trim().to_ascii_uppercase()))
            .collect()),
        Vr::Is | Vr::Ds | Vr::Sv | Vr::Uv => {
            Ok(text.split('\\').map(|part| numeric_value(part, options.numbers)).collect())
        }
        Vr::Us | Vr::Ul | Vr::Sl | Vr::Ss => {
            Ok(text.split('\\').filter(|p| !p.is_empty()).map(|part| parse_int_value(part, vr)).collect())
        }
        Vr::Fl | Vr::Fd => {
            Ok(text.split('\\').filter(|p| !p.is_empty()).map(parse_float_value).collect())
        }
        _ => Ok(text.split('\\').map(|part| Value::String(part.to_string())).collect()),
    }
}

/// Value arrays for elements held as bytes (parsed binary VRs).
fn raw_value_array(raw: &SharedBuffer, vr: Vr, _tag: &Tag) -> Vec<Value> {
    let bytes = raw.data();
    let bytes: &[u8] = bytes.as_ref();
    let unit = vr.unit_size().max(1);
    match vr {
        Vr::At => bytes
            .chunks_exact(4)
            .map(|chunk| {
                let group = u16::from_le_bytes([chunk[0], chunk[1]]);
                let element = u16::from_le_bytes([chunk[2], chunk[3]]);
                Value::String(format!("{group:04X}{element:04X}"))
            })
            .collect(),
        _ if vr.is_string() => {
            let text = String::from_utf8_lossy(bytes);
            let text = text.trim_end_matches(['\0', ' ']).to_string();
            text.split('\\')
                .filter(|part| !part.is_empty())
                .map(|part| Value::String(part.to_string()))
                .collect()
        }
        _ => bytes
            .chunks_exact(unit)
            .map(|chunk| binary_number(chunk, vr).unwrap_or(Value::Null))
            .collect(),
    }
}

fn binary_number(chunk: &[u8], vr: Vr) -> Option<Value> {
    // integers become integer JSON numbers; only the float VRs use f64, so
    // "512" is not written as "512.0"
    let value = match vr {
        Vr::Us if chunk.len() >= 2 => return Some(Value::Number(u16::from_le_bytes([chunk[0], chunk[1]]).into())),
        Vr::Ss if chunk.len() >= 2 => return Some(Value::Number(i16::from_le_bytes([chunk[0], chunk[1]]).into())),
        Vr::Ul if chunk.len() >= 4 => {
            return Some(Value::Number(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).into()))
        }
        Vr::Sl if chunk.len() >= 4 => {
            return Some(Value::Number(i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).into()))
        }
        Vr::Fl if chunk.len() >= 4 => f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64,
        Vr::Fd if chunk.len() >= 8 => f64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]),
        _ => return None,
    };
    Number::from_f64(value).map(Value::Number)
}

fn parse_int_value(part: &str, vr: Vr) -> Value {
    if matches!(vr, Vr::Us | Vr::Ul) {
        return part
            .parse::<u64>()
            .ok()
            .and_then(|v| Number::from_u128(v as u128))
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(part.to_string()));
    }
    part.parse::<i64>()
        .ok()
        .and_then(|v| Number::from_i128(v as i128))
        .map(Value::Number)
        .unwrap_or_else(|| Value::String(part.to_string()))
}

fn parse_float_value(part: &str) -> Value {
    part.parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .map(Value::Number)
        .unwrap_or_else(|| Value::String(part.to_string()))
}

/// IS/DS/SV/UV rendering (§1.2).
fn numeric_value(part: &str, mode: NumberMode) -> Value {
    let trimmed = part.trim();
    match mode {
        NumberMode::AsString => Value::String(part.to_string()),
        NumberMode::AsNumber => match trimmed.parse::<f64>() {
            // NaN and infinities always stay strings (§1.2)
            Ok(value) if value.is_finite() => {
                integer_aware_number(trimmed, value).unwrap_or_else(|| Value::String(part.to_string()))
            }
            _ => Value::String(part.to_string()),
        },
        NumberMode::PreferablyAsNumber => {
            // write a number only when reading it back yields the same text:
            // "1.50" stays a string because a number would lose the zero
            match trimmed.parse::<f64>() {
                Ok(value) if value.is_finite() && format_number_like(value) == trimmed => {
                    integer_aware_number(trimmed, value).unwrap_or_else(|| Value::String(part.to_string()))
                }
                _ => Value::String(part.to_string()),
            }
        }
    }
}

/// A JSON number that stays an integer when the text is an integer.
fn integer_aware_number(text: &str, value: f64) -> Option<Value> {
    if !text.contains('.') && !text.contains('e') && !text.contains('E') {
        if let Ok(integer) = text.parse::<i64>() {
            return Some(Value::Number(integer.into()));
        }
        if let Ok(integer) = text.parse::<u64>() {
            return Some(Value::Number(integer.into()));
        }
    }
    Number::from_f64(value).map(Value::Number)
}

/// Formats a float the way serde_json would, for the lossless check.
fn format_number_like(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e16 {
        return format!("{}", value as i64);
    }
    format!("{value}")
}

/// PN as the three-group object (§1.1).
fn person_name_value(text: &str) -> Value {
    let name = PersonName::parse(text);
    let mut object = Map::new();
    let alphabetic = name.alphabetic();
    if !alphabetic.is_empty() {
        object.insert("Alphabetic".to_string(), Value::String(alphabetic.to_string()));
    }
    if let Some(ideographic) = name.ideographic().filter(|s| !s.is_empty()) {
        object.insert("Ideographic".to_string(), Value::String(ideographic.to_string()));
    }
    if let Some(phonetic) = name.phonetic().filter(|s| !s.is_empty()) {
        object.insert("Phonetic".to_string(), Value::String(phonetic.to_string()));
    }
    Value::Object(object)
}

/// Deserializes a dataset from DICOM JSON (§1.4).
pub fn json_to_dataset(json: &str, _options: &JsonOptions) -> Result<Dataset> {
    let value: Value = serde_json::from_str(json)?;
    value_to_dataset(&value)
}

/// Deserializes a dataset from a JSON value tree (§1.4).
pub fn value_to_dataset(value: &Value) -> Result<Dataset> {
    let Value::Object(object) = value else {
        return Err(JsonError::Unsupported("a DICOM JSON dataset must be an object"));
    };
    let mut dataset = Dataset::new();

    for (key, attribute) in object {
        let Some(tag) = parse_tag_key(key) else {
            // unknown keys (keywords, private extensions) are skipped (§1.3)
            continue;
        };
        let Some(item) = attribute_to_item(tag, attribute)? else {
            continue;
        };
        dataset.insert(item)?;
    }
    Ok(dataset)
}

fn attribute_to_item(tag: Tag, attribute: &Value) -> Result<Option<DicomItem>> {
    let Value::Object(object) = attribute else {
        return Err(JsonError::Malformed {
            tag: tag.to_json_string(),
            reason: "an attribute must be an object".to_string(),
        });
    };
    // "vr" may sit at any property position (§1.3)
    let Some(vr_code) = object.get("vr").and_then(Value::as_str) else {
        return Err(JsonError::MissingVr { tag: tag.to_json_string() });
    };
    let vr = Vr::parse_str(vr_code)
        .map_err(|_| JsonError::UnknownVr { tag: tag.to_json_string(), vr: vr_code.to_string() })?;

    if object.contains_key("BulkDataURI") {
        // ponytail: fetching a bulk URI needs the store layer that owns it;
        // until that exists the read path reports it instead of guessing (JSON-02).
        return Err(JsonError::Unsupported("BulkDataURI retrieval is not implemented yet"));
    }

    if let Some(encoded) = object.get("InlineBinary").and_then(Value::as_str) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| JsonError::Malformed { tag: tag.to_json_string(), reason: format!("bad base64: {e}") })?;
        return Ok(Some(DicomItem::Element(DicomElement::new_bytes(
            tag,
            vr,
            Arc::new(MemoryBuffer::from(bytes)),
        ))));
    }

    let values = match object.get("Value") {
        Some(Value::Array(values)) => values,
        // an attribute without a carrier is an empty element (§1.1)
        None | Some(Value::Null) => return Ok(Some(DicomItem::Element(DicomElement::new_empty(tag, vr)))),
        Some(other) => {
            return Err(JsonError::Malformed {
                tag: tag.to_json_string(),
                reason: format!("\"Value\" must be an array, got {}", type_name(other)),
            })
        }
    };

    if vr == Vr::Sq {
        let mut sequence = DicomSequence::new(tag);
        for item in values {
            sequence.push(value_to_dataset(item)?);
        }
        return Ok(Some(DicomItem::Sequence(sequence)));
    }

    check_value_vr(vr, &tag)?;
    let text = values_to_text(vr, values, &tag)?;
    let element = text_element(tag, vr, text)?;
    Ok(Some(DicomItem::Element(element)))
}

/// Builds an element from its text form, matching how the VR stores values.
///
/// Only string VRs live as `Str` in the element model; AT (4-byte pairs) and
/// the numeric binary VRs are byte-backed, so they are built from bytes here.
fn text_element(tag: Tag, vr: Vr, text: String) -> Result<DicomElement> {
    if vr == Vr::At {
        let mut bytes = Vec::new();
        for part in text.split('\\').filter(|part| !part.is_empty()) {
            let parsed = Tag::parse(part).map_err(|_| JsonError::Malformed {
                tag: tag.to_json_string(),
                reason: format!("AT value \"{part}\" is not a tag"),
            })?;
            bytes.extend_from_slice(&parsed.group().to_le_bytes());
            bytes.extend_from_slice(&parsed.element().to_le_bytes());
        }
        return Ok(DicomElement::new_bytes(tag, vr, Arc::new(MemoryBuffer::from(bytes))));
    }
    if vr == Vr::Pn || !vr.is_string() {
        let bytes = encode_numeric_or_text(vr, &text)?;
        return Ok(DicomElement::new_bytes(tag, vr, Arc::new(MemoryBuffer::from(bytes))));
    }
    Ok(DicomElement::new_str(tag, vr, text))
}

/// Encodes the text form of a binary numeric VR (or PN) back to bytes.
fn encode_numeric_or_text(vr: Vr, text: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for part in text.split('\\') {
        if part.is_empty() {
            continue;
        }
        let malformed = |kind: &str| JsonError::Malformed {
            tag: String::new(),
            reason: format!("\"{part}\" is not a valid {kind}"),
        };
        match vr {
            Vr::Us => bytes.extend_from_slice(&part.parse::<u16>().map_err(|_| malformed("US"))?.to_le_bytes()),
            Vr::Ss => bytes.extend_from_slice(&part.parse::<i16>().map_err(|_| malformed("SS"))?.to_le_bytes()),
            Vr::Ul => bytes.extend_from_slice(&part.parse::<u32>().map_err(|_| malformed("UL"))?.to_le_bytes()),
            Vr::Sl => bytes.extend_from_slice(&part.parse::<i32>().map_err(|_| malformed("SL"))?.to_le_bytes()),
            Vr::Fl => bytes.extend_from_slice(&part.parse::<f32>().map_err(|_| malformed("FL"))?.to_le_bytes()),
            Vr::Fd => bytes.extend_from_slice(&part.parse::<f64>().map_err(|_| malformed("FD"))?.to_le_bytes()),
            // PN keeps its UTF-8 text; the charset layer encodes for the wire
            _ => bytes.extend_from_slice(part.as_bytes()),
        }
    }
    Ok(bytes)
}

/// A `Value` carrier needs a string-backed VR; binary VRs use InlineBinary.
fn check_value_vr(vr: Vr, tag: &Tag) -> Result<()> {
    if vr.is_string() || matches!(vr, Vr::Us | Vr::Sl | Vr::Ss | Vr::Ul | Vr::Fl | Vr::Fd | Vr::At) {
        return Ok(());
    }
    Err(JsonError::Malformed {
        tag: tag_key(tag),
        reason: format!("VR {} must use InlineBinary, not Value", vr.code()),
    })
}

/// Joins the JSON values into the `\`-separated internal text form (§1.3).
fn values_to_text(vr: Vr, values: &[Value], tag: &Tag) -> Result<String> {
    let mut parts = Vec::with_capacity(values.len());
    for value in values {
        parts.push(value_to_text(vr, value, tag)?);
    }
    Ok(parts.join("\\"))
}

fn value_to_text(vr: Vr, value: &Value, tag: &Tag) -> Result<String> {
    match value {
        Value::Null => Ok(String::new()),
        Value::String(text) => {
            if vr == Vr::At {
                return Ok(text.trim().to_ascii_uppercase());
            }
            Ok(text.clone())
        }
        Value::Number(number) => Ok(number.to_string()),
        // PN groups (§1.1)
        Value::Object(object) if vr == Vr::Pn => {
            let mut groups = Vec::new();
            for key in ["Alphabetic", "Ideographic", "Phonetic"] {
                if let Some(part) = object.get(key).and_then(Value::as_str) {
                    groups.push(part.to_string());
                }
            }
            Ok(groups.join("="))
        }
        Value::Object(_) => Err(JsonError::Malformed {
            tag: tag_key(tag),
            reason: "an object value is only valid for PN".to_string(),
        }),
        Value::Array(_) => Err(JsonError::Malformed {
            tag: tag_key(tag),
            reason: "a nested array is not a scalar value".to_string(),
        }),
        Value::Bool(_) => Err(JsonError::Malformed {
            tag: tag_key(tag),
            reason: "a boolean is not a DICOM value".to_string(),
        }),
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radicom::model::transfer_syntax::syntaxes;

    fn sample() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0008, 0x0016), Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        ds.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Doe^John");
        ds.insert_str(Tag::new(0x0010, 0x0020), Vr::Lo, "PID-1");
        ds.insert_str(Tag::new(0x0008, 0x0060), Vr::Cs, "CT");
        ds.insert_str(Tag::new(0x0020, 0x0013), Vr::Is, "42");
        ds.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
    }

    #[test]
    fn tags_use_eight_hex_digits() {
        assert_eq!(tag_key(&Tag::new(0x0010, 0x0010)), "00100010");
        assert_eq!(tag_key(&Tag::new(0x7FE0, 0x0010)), "7FE00010");
    }

    #[test]
    fn a_dataset_serializes_with_vr_and_value() {
        let json = dataset_to_json(&sample(), &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        let patient = &value["00100010"];
        assert_eq!(patient["vr"], "PN");
        assert_eq!(patient["Value"][0]["Alphabetic"], "Doe^John", "PN uses the three-group form");
        assert_eq!(value["00100020"]["Value"][0], "PID-1");
        assert_eq!(value["00080060"]["vr"], "CS");
    }

    #[test]
    fn keys_are_upper_case_hex() {
        let json = dataset_to_json(&sample(), &JsonOptions::default()).unwrap();
        assert!(json.contains("\"00080016\""), "got {json}");
    }

    #[test]
    fn round_trip_preserves_the_dataset() {
        let ds = sample();
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert_eq!(back.get_str(Tag::new(0x0010, 0x0010)).unwrap(), "Doe^John");
        assert_eq!(back.get_str(Tag::new(0x0010, 0x0020)).unwrap(), "PID-1");
        assert_eq!(back.get_str(Tag::new(0x0008, 0x0016)).unwrap(), "1.2.840.10008.5.1.4.1.1.7");
        assert_eq!(back.get_str(Tag::new(0x0020, 0x0013)).unwrap(), "42");
        assert_eq!(back.len(), ds.len(), "no element is lost");
    }

    #[test]
    fn person_name_groups_round_trip() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Yamada^Tarou=山田^太郎=やまだ^たろう");
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["00100010"]["Value"][0]["Ideographic"], "山田^太郎");
        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert_eq!(
            back.get_str(Tag::new(0x0010, 0x0010)).unwrap(),
            "Yamada^Tarou=山田^太郎=やまだ^たろう"
        );
    }

    #[test]
    fn multi_value_strings_split_on_backslash() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0008, 0x0061), Vr::Cs, "CT\\MR");
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["00080061"]["Value"], serde_json::json!(["CT", "MR"]));
        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert_eq!(back.get_str(Tag::new(0x0008, 0x0061)).unwrap(), "CT\\MR");
    }

    #[test]
    fn empty_elements_omit_the_value_carrier() {
        let mut ds = Dataset::new();
        ds.insert_empty(Tag::new(0x0010, 0x0020), Vr::Lo);
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["00100020"]["vr"], "LO");
        assert!(value["00100020"].get("Value").is_none(), "empty elements carry no Value");

        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert!(back.get_element(Tag::new(0x0010, 0x0020)).unwrap().is_empty());
    }

    #[test]
    fn binary_values_use_inline_binary() {
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::new(0x7FE0, 0x0010),
            Vr::Ob,
            Arc::new(MemoryBuffer::from(vec![1u8, 2, 3, 4])),
        )));
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["7FE00010"]["vr"], "OB");
        assert_eq!(value["7FE00010"]["InlineBinary"], "AQIDBA==");

        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        let bytes = back.get_element(Tag::new(0x7FE0, 0x0010)).unwrap();
        assert_eq!(bytes.value().as_buffer().unwrap().data().to_vec(), vec![1u8, 2, 3, 4]);
    }

    #[test]
    fn parsed_binary_integers_serialize_as_numbers() {
        // a US element parsed from the wire is held as bytes
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::new(0x0028, 0x0010),
            Vr::Us,
            Arc::new(MemoryBuffer::from(512u16.to_le_bytes().to_vec())),
        )));
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["00280010"]["vr"], "US");
        assert_eq!(value["00280010"]["Value"][0], 512);

        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        let element = back.get_element(Tag::new(0x0028, 0x0010)).unwrap();
        assert_eq!(
            element.value().as_buffer().unwrap().data().as_ref(),
            512u16.to_le_bytes(),
            "a US element stays byte-backed after a round-trip"
        );
        assert_eq!(element.value().as_buffer().unwrap().size(), 2, "one US value");
    }

    #[test]
    fn at_values_are_eight_hex_digits_in_either_case() {
        // AT is a binary VR, so it is held as bytes (as the parser builds it)
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::new(0x0008, 0x103E),
            Vr::At,
            Arc::new(MemoryBuffer::from(vec![0x10, 0x00, 0x10, 0x00])),
        )));
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["0008103E"]["Value"][0], "00100010");

        let lower = r#"{"0008103E":{"vr":"AT","Value":["00100010"]}}"#;
        let back = json_to_dataset(lower, &JsonOptions::default()).unwrap();
        let element = back.get_element(Tag::new(0x0008, 0x103E)).unwrap();
        let bytes = element.value().as_buffer().unwrap();
        assert_eq!(bytes.data().as_ref(), &[0x10, 0x00, 0x10, 0x00], "AT round-trips as 4-byte pairs");
    }

    #[test]
    fn sequences_recurse() {
        let mut ds = Dataset::new();
        let mut item = Dataset::new();
        item.insert_str(Tag::new(0x0008, 0x0100), Vr::Sh, "113100");
        ds.insert_sequence(Tag::new(0x0040, 0xA730)).push(item);

        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["0040A730"]["vr"], "SQ");
        assert_eq!(value["0040A730"]["Value"][0]["00080100"]["Value"][0], "113100");

        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        let sequence = back.get_sequence(Tag::new(0x0040, 0xA730)).unwrap();
        assert_eq!(sequence.items().len(), 1);
        assert_eq!(sequence.items()[0].get_str(Tag::new(0x0008, 0x0100)).unwrap(), "113100");
    }

    #[test]
    fn fragment_sequences_are_rejected_with_a_hint() {
        let mut ds = Dataset::new();
        ds.insert_fragments(Tag::new(0x7FE0, 0x0010), Vr::Ob);
        let error = dataset_to_json(&ds, &JsonOptions::default()).unwrap_err();
        assert!(matches!(error, JsonError::Fragments));
        assert!(error.to_string().contains("transcode"), "the message suggests transcoding: {error}");
    }

    #[test]
    fn numeric_modes_control_is_and_ds_rendering() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0020, 0x0013), Vr::Is, "42");
        ds.insert_str(Tag::new(0x0018, 0x0050), Vr::Ds, "1.50");

        let as_number = JsonOptions { numbers: NumberMode::AsNumber, ..Default::default() };
        let value: Value = serde_json::from_str(&dataset_to_json(&ds, &as_number).unwrap()).unwrap();
        assert_eq!(value["00200013"]["Value"][0], 42, "numbers by request");
        assert_eq!(value["00180050"]["Value"][0], 1.5, "numbers by request");

        let as_string = JsonOptions { numbers: NumberMode::AsString, ..Default::default() };
        let value: Value = serde_json::from_str(&dataset_to_json(&ds, &as_string).unwrap()).unwrap();
        assert_eq!(value["00200013"]["Value"][0], "42");
        assert_eq!(value["00180050"]["Value"][0], "1.50", "the string form is preserved");

        // the default keeps DS trailing zeros, but writes plain integers as numbers
        let value: Value = serde_json::from_str(&dataset_to_json(&ds, &JsonOptions::default()).unwrap()).unwrap();
        assert_eq!(value["00200013"]["Value"][0], 42);
        assert_eq!(value["00180050"]["Value"][0], "1.50", "1.50 would lose its trailing zero");
    }

    #[test]
    fn nan_and_infinity_stay_strings() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0018, 0x0050), Vr::Ds, "NaN");
        let options = JsonOptions { numbers: NumberMode::AsNumber, ..Default::default() };
        let value: Value = serde_json::from_str(&dataset_to_json(&ds, &options).unwrap()).unwrap();
        assert_eq!(value["00180050"]["Value"][0], "NaN", "NaN can never be a JSON number");
    }

    #[test]
    fn reading_accepts_numbers_for_numeric_vrs() {
        let json = r#"{"00200013":{"vr":"IS","Value":[42]}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        assert_eq!(ds.get_str(Tag::new(0x0020, 0x0013)).unwrap(), "42");
    }

    #[test]
    fn reading_tolerates_unknown_properties_and_unordered_vr() {
        // "vr" after the value, plus an extra property that must be ignored
        let json = r#"{"00100020":{"Value":["PID-1"],"extra":true,"vr":"LO"}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        assert_eq!(ds.get_str(Tag::new(0x0010, 0x0020)).unwrap(), "PID-1");
    }

    #[test]
    fn reading_skips_unknown_keys() {
        let json = r#"{"PatientID":{"vr":"LO","Value":["PID-1"]},"00100020":{"vr":"LO","Value":["PID-1"]}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        assert_eq!(ds.len(), 1, "the keyword key is skipped, the tag key is read");
    }

    #[test]
    fn a_missing_vr_is_a_hard_error() {
        let json = r#"{"00100020":{"Value":["PID-1"]}}"#;
        match json_to_dataset(json, &JsonOptions::default()) {
            Err(JsonError::MissingVr { tag }) => assert_eq!(tag, "00100020"),
            other => panic!("expected MissingVr, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_vr_is_a_hard_error() {
        let json = r#"{"00100020":{"vr":"ZZ","Value":["x"]}}"#;
        assert!(matches!(json_to_dataset(json, &JsonOptions::default()), Err(JsonError::UnknownVr { .. })));
    }

    #[test]
    fn indented_output_is_readable() {
        let options = JsonOptions { format_indented: true, ..Default::default() };
        let json = dataset_to_json(&sample(), &options).unwrap();
        assert!(json.contains('\n'), "pretty printing inserts newlines");
        assert!(json_to_dataset(&json, &JsonOptions::default()).is_ok());
    }

    #[test]
    fn a_non_object_root_is_rejected() {
        assert!(json_to_dataset("[1,2,3]", &JsonOptions::default()).is_err());
    }

    #[test]
    fn bulk_data_uri_reports_unsupported() {
        let json = r#"{"7FE00010":{"vr":"OB","BulkDataURI":"http://example/x"}}"#;
        match json_to_dataset(json, &JsonOptions::default()) {
            Err(JsonError::Unsupported(message)) => assert!(message.contains("BulkDataURI"), "got {message}"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_properties_take_the_last_value() {
        // serde_json itself keeps the last duplicate, which is the rule (§1.3)
        let json = r#"{"00100020":{"vr":"LO","Value":["first"]},"00100020":{"vr":"LO","Value":["second"]}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        assert_eq!(ds.get_str(Tag::new(0x0010, 0x0020)).unwrap(), "second");
    }
}
