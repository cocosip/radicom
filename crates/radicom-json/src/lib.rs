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
//! Fragment sequences are deliberately not representable (§12.1 of
//! docs/architecture.md §12): they return an error that suggests
//! transcoding, matching fo-dicom.
//!
//! One deliberate divergence from fo-dicom's `JsonDicomConverter` defaults:
//! `NumberMode::PreferablyAsNumber` is the default here, not `AsNumber`. An
//! IS/DS value is written as a number only when the number's own text form is
//! identical, so `"0001"` stays a string where fo-dicom writes `1`; this keeps
//! the JSON lossless. `NumberMode::AsNumber` matches fo-dicom's behaviour.

use base64::Engine as _;
use radicom::model::buffer::{MemoryBuffer, SharedBuffer};
use std::borrow::Cow;

pub mod xml;

use radicom::model::dataset::Dataset;
use radicom::model::element::{DicomElement, ElementValue};
use radicom::model::item::{DicomItem, DicomSequence};
use radicom::model::person_name::PersonName;
use radicom::model::private_creator::PrivateCreator;
use radicom::model::tag::Tag;
use radicom::model::vr::Vr;
use serde_json::{Map, Number, Value};
use std::sync::Arc;
pub use xml::dataset_to_xml;

/// Errors of the JSON model (§12.1).
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
    #[error("deferred value read failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("dataset error: {0}")]
    Dataset(#[from] radicom::Error),
}

/// Result alias for the JSON crate.
pub type Result<T> = std::result::Result<T, JsonError>;

/// How IS/DS/SV/UV values are written (§12.1).
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

/// Names a `BulkDataURI` for a deferred value (PS3.18 Annex F).
///
/// Called for a non-empty binary element whose buffer is not in memory (a
/// `FileWindow`, `StreamWindow` or `TempFileSpill`), with the element's tag and
/// buffer. Returning a non-blank URI writes `{"BulkDataURI": …}` and never
/// materialises the value; `None` or a blank URI falls back to `InlineBinary`.
pub type BulkDataUriProvider = dyn Fn(&Tag, &SharedBuffer) -> Option<String> + Send + Sync;

/// Serialization options (§12.1).
#[derive(Clone)]
pub struct JsonOptions {
    /// Non-standard: use attribute keywords instead of tags as keys. Only
    /// affects writing, and only when the dictionary knows the tag.
    pub tags_as_keywords: bool,
    /// Validate every element against its VR rules after reading (fo-dicom
    /// `autoValidate`). The crate-wide validation switch still applies, so a
    /// scope that disabled validation keeps parsing tolerant.
    pub validate_on_read: bool,
    pub numbers: NumberMode,
    pub format_indented: bool,
    /// Non-standard: also emit each attribute's dictionary keyword as a
    /// `"keyword"` property (fo-dicom `WriteKeyword`). Unknown, masked and
    /// private tags carry no annotation.
    pub write_keyword: bool,
    /// Non-standard: also emit each attribute's dictionary name as a `"name"`
    /// property (fo-dicom `WriteName`).
    pub write_name: bool,
    /// Names a URI for deferred values instead of materialising them
    /// (PS3.18 Annex F `BulkDataURI`). Only non-memory buffers are offered, so
    /// an in-memory value always becomes `InlineBinary` and the output of an
    /// unconfigured writer does not change.
    pub bulk_data_uri: Option<Arc<BulkDataUriProvider>>,
}

impl std::fmt::Debug for JsonOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonOptions")
            .field("tags_as_keywords", &self.tags_as_keywords)
            .field("validate_on_read", &self.validate_on_read)
            .field("numbers", &self.numbers)
            .field("format_indented", &self.format_indented)
            .field("write_keyword", &self.write_keyword)
            .field("write_name", &self.write_name)
            .field("bulk_data_uri", &self.bulk_data_uri.is_some())
            .finish()
    }
}

impl Default for JsonOptions {
    fn default() -> Self {
        JsonOptions {
            tags_as_keywords: false,
            validate_on_read: false,
            numbers: NumberMode::PreferablyAsNumber,
            format_indented: false,
            write_keyword: false,
            write_name: false,
            bulk_data_uri: None,
        }
    }
}

/// The 8-character hex key of a tag (§12.1).
pub fn tag_key(tag: &Tag) -> String {
    tag.to_json_string()
}

/// Parses a tag key: an 8-hex-digit tag, or a dictionary keyword (fo-dicom
/// `ParseTag`: `DicomDictionary.Default[tagstr]`). Anything else is skipped on
/// read.
fn parse_tag_key(key: &str) -> Option<Tag> {
    if let Ok(tag) = Tag::parse(key) {
        return Some(tag);
    }
    // private keywords resolve to a placeholder creator-scoped tag, which is
    // not the tag the writer would have produced; only public keywords count.
    radicom::default_dictionary()
        .tag_by_keyword(key)
        .filter(|tag| tag.private_creator().is_none())
}

/// Serializes a dataset to DICOM JSON (§12.1).
pub fn dataset_to_json(dataset: &Dataset, options: &JsonOptions) -> Result<String> {
    let value = dataset_to_value(dataset, options)?;
    if options.format_indented {
        Ok(serde_json::to_string_pretty(&value)?)
    } else {
        Ok(serde_json::to_string(&value)?)
    }
}

/// Serializes a collection of datasets as a DICOM JSON array (§12.1).
pub fn datasets_to_json(datasets: &[Dataset], options: &JsonOptions) -> Result<String> {
    let values = datasets
        .iter()
        .map(|dataset| dataset_to_value(dataset, options))
        .collect::<Result<Vec<_>>>()?;
    let value = Value::Array(values);
    if options.format_indented {
        Ok(serde_json::to_string_pretty(&value)?)
    } else {
        Ok(serde_json::to_string(&value)?)
    }
}

/// Serializes a dataset to a JSON value tree (§12.1).
///
/// Group-length elements `(GGGG,0000)` are skipped, here and in every nested
/// item: PS3.18 Annex F has no representation for them and they are redundant
/// in JSON (JSON-01).
pub fn dataset_to_value(dataset: &Dataset, options: &JsonOptions) -> Result<Value> {
    let mut object = Map::new();
    for item in dataset.items() {
        if item.tag().element() == 0x0000 {
            continue;
        }
        match item {
            DicomItem::Element(element) => {
                let mut value = element_to_value(element, options)?;
                annotate(&mut value, element.tag(), options);
                object.insert(key_for(element.tag(), options), value);
            }
            DicomItem::Sequence(sequence) => {
                let mut value = sequence_to_value(sequence, options)?;
                annotate(&mut value, sequence.tag(), options);
                object.insert(key_for(sequence.tag(), options), value);
            }
            DicomItem::Fragments(_) => return Err(JsonError::Fragments),
        }
    }
    Ok(Value::Object(object))
}

/// The dictionary keyword/name of a tag that fo-dicom may use as a JSON key or
/// annotation. Unknown, blank-keyword, masked/repeating-group and private tags
/// fall back to the hex tag instead: their keyword is shared by many tags
/// (`Unknown`, `PrivateCreator`, `OverlayData`) and would collide in the JSON
/// object (fo-dicom `JsonDicomConverter.cs:116-129`).
fn keyword_and_name(tag: &Tag) -> Option<(&'static str, &'static str)> {
    if tag.is_private() {
        return None;
    }
    let entry = radicom::default_dictionary().entry(tag);
    if entry.mask.is_some() || entry.keyword.is_empty() || entry.keyword == "Unknown" {
        return None;
    }
    Some((entry.keyword, entry.name))
}

/// Adds the non-standard per-attribute `keyword`/`name` annotations
/// (fo-dicom `WriteKeyword`/`WriteName`).
fn annotate(value: &mut Value, tag: &Tag, options: &JsonOptions) {
    if !options.write_keyword && !options.write_name {
        return;
    }
    let Some((keyword, name)) = keyword_and_name(tag) else {
        return;
    };
    let Value::Object(object) = value else {
        return;
    };
    if options.write_keyword {
        object.insert("keyword".to_string(), Value::String(keyword.to_string()));
    }
    if options.write_name {
        object.insert("name".to_string(), Value::String(name.to_string()));
    }
}

fn key_for(tag: &Tag, options: &JsonOptions) -> String {
    if options.tags_as_keywords {
        if let Some((keyword, _)) = keyword_and_name(tag) {
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
    // an empty sequence carries no Value, like fo-dicom (§12.1, PS3.18 Annex F)
    if !values.is_empty() {
        object.insert("Value".to_string(), Value::Array(values));
    }
    Ok(Value::Object(object))
}

/// True for VRs carried as a base64 `InlineBinary` (§12.1).
fn is_binary(vr: Vr) -> bool {
    matches!(
        vr,
        Vr::Ob | Vr::Od | Vr::Of | Vr::Ol | Vr::Ov | Vr::Ow | Vr::Un
    )
}

/// VRs for which PS3.18 F.2.2 permits a `BulkDataURI` carrier: DS, FL, FD,
/// IS, LT, OB, OD, OF, OL, OV, OW, SL, SS, ST, SV, UC, UL, UN, US, UT, UV.
fn allows_bulk_data_uri(vr: Vr) -> bool {
    matches!(
        vr,
        Vr::Ds
            | Vr::Fl
            | Vr::Fd
            | Vr::Is
            | Vr::Lt
            | Vr::Ob
            | Vr::Od
            | Vr::Of
            | Vr::Ol
            | Vr::Ov
            | Vr::Ow
            | Vr::Sl
            | Vr::Ss
            | Vr::St
            | Vr::Sv
            | Vr::Uc
            | Vr::Ul
            | Vr::Un
            | Vr::Us
            | Vr::Ut
            | Vr::Uv
    )
}

fn element_to_value(element: &DicomElement, options: &JsonOptions) -> Result<Value> {
    let vr = element.vr();
    let mut object = Map::new();
    object.insert("vr".to_string(), Value::String(vr.code().to_string()));

    // PS3.18 F.2.2 lists the VRs a BulkDataURI may carry; for any other VR the
    // value must be in `Value`, so a caller that deferred it by URI cannot be
    // honoured.
    if let Some(buffer) = element.value().as_buffer() {
        if let Some(uri) = buffer.bulk_uri() {
            if !allows_bulk_data_uri(vr) {
                return Err(JsonError::Unsupported(
                    "BulkDataURI is not permitted for this VR (PS3.18 F.2.2)",
                ));
            }
            object.insert("BulkDataURI".to_string(), Value::String(uri.to_string()));
            return Ok(Value::Object(object));
        }
    }

    if is_binary(vr) {
        if let Some(buffer) = element.value().as_buffer() {
            // a deferred value goes out as a URI when the caller names one, and
            // is never materialised for it (§12.1). An empty value has nothing to
            // defer and a blank URI is not a carrier, so both fall through.
            if buffer.size() > 0 && !buffer.is_memory() {
                let uri = options
                    .bulk_data_uri
                    .as_ref()
                    .and_then(|provider| provider(element.tag(), buffer))
                    .filter(|uri| !uri.trim().is_empty());
                if let Some(uri) = uri {
                    object.insert("BulkDataURI".to_string(), Value::String(uri));
                    return Ok(Value::Object(object));
                }
            }
            // an empty binary value carries no carrier at all (§12.1)
            if buffer.size() > 0 {
                let encoded =
                    base64::engine::general_purpose::STANDARD.encode(buffer.data()?.as_ref());
                object.insert("InlineBinary".to_string(), Value::String(encoded));
            }
        }
        return Ok(Value::Object(object));
    }

    if vr == Vr::Sq {
        // an SQ stored as an element (a length-0 sequence) serializes as a bare
        // {"vr":"SQ"}, with no Value array (§12.1)
        return Ok(Value::Object(object));
    }

    let values = element_values(element, vr, options)?;
    // an empty element omits Value entirely (§12.1)
    if !values.is_empty() {
        object.insert("Value".to_string(), Value::Array(values));
    }
    Ok(Value::Object(object))
}

/// Builds the `Value` array for a non-binary element (§12.1).
fn element_values(element: &DicomElement, vr: Vr, options: &JsonOptions) -> Result<Vec<Value>> {
    // bytes that are not a binary VR come from a parsed element (e.g. US);
    // strings are the other possible representation
    if let Some(raw) = element.value().as_buffer() {
        return raw_value_array(raw, vr, element.tag(), options);
    }
    let text = element.string_value();
    if text.is_empty() {
        return Ok(Vec::new());
    }

    match vr {
        // an empty PN value component is `null`, not an empty object
        Vr::Pn => Ok(text
            .split('\\')
            .map(|part| {
                if part.is_empty() {
                    Value::Null
                } else {
                    person_name_value(part)
                }
            })
            .collect()),
        Vr::At => Ok(text
            .split('\\')
            .filter(|part| !part.is_empty())
            .map(|part| Value::String(part.trim().to_ascii_uppercase()))
            .collect()),
        // A zero-length component is `null`, not `""` (§12.1, PS3.18 Annex F),
        // for the numeric VRs too — dropping it would silently change the VM.
        Vr::Is | Vr::Ds => Ok(text
            .split('\\')
            .map(|part| {
                if part.trim().is_empty() {
                    Value::Null
                } else {
                    numeric_value(part, options.numbers)
                }
            })
            .collect()),
        // The fixed-width binary numeric VRs are always byte-backed (see
        // `DicomElement::new_str`, which forbids them), so they are handled by
        // `raw_value_array` and never reach this text path. Keep the text form
        // without a `null` branch: a zero-length component cannot be represented
        // for a fixed-width VR and the read side rightly rejects it (FJ-D-06).
        Vr::Us | Vr::Ul | Vr::Sl | Vr::Ss => Ok(text
            .split('\\')
            .map(|part| parse_int_value(part, vr))
            .collect()),
        Vr::Fl | Vr::Fd => Ok(text.split('\\').map(parse_float_value).collect()),
        // a zero-length component is `null`, not `""` (§12.1, PS3.18 Annex F)
        _ => Ok(text
            .split(|c| c == '\\' && vr.is_multi_value())
            .map(|part| {
                if part.is_empty() {
                    Value::Null
                } else {
                    Value::String(part.to_string())
                }
            })
            .collect()),
    }
}

/// Value arrays for elements held as bytes (parsed binary VRs).
fn raw_value_array(
    raw: &SharedBuffer,
    vr: Vr,
    _tag: &Tag,
    options: &JsonOptions,
) -> Result<Vec<Value>> {
    let bytes = raw.data()?;
    let bytes: &[u8] = bytes.as_ref();
    let unit = vr.unit_size().max(1);
    if matches!(
        vr,
        Vr::At | Vr::Us | Vr::Ss | Vr::Ul | Vr::Sl | Vr::Fl | Vr::Fd | Vr::Sv | Vr::Uv
    ) && !bytes.len().is_multiple_of(unit)
    {
        return Err(JsonError::Malformed {
            tag: _tag.to_string(),
            reason: format!(
                "{} value length {} is not a multiple of {}",
                vr,
                bytes.len(),
                unit
            ),
        });
    }
    Ok(match vr {
        Vr::At => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| {
                let group = u16::from_le_bytes([chunk[0], chunk[1]]);
                let element = u16::from_le_bytes([chunk[2], chunk[3]]);
                Value::String(format!("{group:04X}{element:04X}"))
            })
            .collect(),
        _ if vr.is_string() => {
            let text = String::from_utf8_lossy(bytes);
            let text = text.trim_end_matches(['\0', ' ']).to_string();
            if text.is_empty() {
                return Ok(Vec::new());
            }
            // PN must use the three-group object form even when byte-backed
            // (§12.1, PS3.18 Annex F.3.1) — a plain string array is not PN.
            if vr == Vr::Pn {
                return Ok(text
                    .split('\\')
                    .map(|part| {
                        if part.is_empty() {
                            Value::Null
                        } else {
                            person_name_value(part)
                        }
                    })
                    .collect());
            }
            // a zero-length component is `null`, not `""` (§12.1, PS3.18 Annex
            // F), exactly like the Str-backed path above: dropping it would
            // silently change the element's VM
            text.split(|c| c == '\\' && vr.is_multi_value())
                .map(|part| {
                    if part.is_empty() {
                        Value::Null
                    } else if matches!(vr, Vr::Is | Vr::Ds | Vr::Sv | Vr::Uv) {
                        // numeric text honors JsonOptions::numbers like the
                        // Str-backed path (FJ-D-05)
                        numeric_value(part, options.numbers)
                    } else {
                        Value::String(part.to_string())
                    }
                })
                .collect()
        }
        _ => bytes
            .chunks_exact(unit)
            .map(|chunk| binary_number(chunk, vr).unwrap_or(Value::Null))
            .collect(),
    })
}

fn binary_number(chunk: &[u8], vr: Vr) -> Option<Value> {
    // integers become integer JSON numbers; only the float VRs use f64, so
    // "512" is not written as "512.0"
    let value = match vr {
        Vr::Us if chunk.len() >= 2 => {
            return Some(Value::Number(
                u16::from_le_bytes([chunk[0], chunk[1]]).into(),
            ))
        }
        Vr::Ss if chunk.len() >= 2 => {
            return Some(Value::Number(
                i16::from_le_bytes([chunk[0], chunk[1]]).into(),
            ))
        }
        Vr::Ul if chunk.len() >= 4 => {
            return Some(Value::Number(
                u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).into(),
            ))
        }
        Vr::Sl if chunk.len() >= 4 => {
            return Some(Value::Number(
                i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).into(),
            ))
        }
        // 64-bit integers stay exact (no f64 hop) (review #12)
        Vr::Sv if chunk.len() >= 8 => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&chunk[..8]);
            return Some(Value::Number(i64::from_le_bytes(bytes).into()));
        }
        Vr::Uv if chunk.len() >= 8 => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&chunk[..8]);
            return Some(Value::Number(u64::from_le_bytes(bytes).into()));
        }
        Vr::Fl if chunk.len() >= 4 => {
            let f = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            // Preserve the f32 shortest decimal form: fo-dicom writes 0.1f32 as
            // "0.1", not the f64 expansion "0.10000000149011612". Re-parsing the
            // Display text keeps serde_json to a Number while emitting the short
            // form. Non-finite values stay as the DICOM JSON strings below.
            return Some(if f.is_finite() {
                let mut text = format!("{f}");
                // Keep FL a JSON float even for integral values (1.0f -> "1.0"),
                // preserving the previous float-typed output; fractional values
                // keep f32-shortest text ("0.1", never the f64 expansion).
                if !(text.contains('.') || text.contains('e') || text.contains('E')) {
                    text.push_str(".0");
                }
                serde_json::from_str::<Value>(&text)
                    .unwrap_or_else(|_| Value::Number(Number::from_f64(f as f64).expect("finite")))
            } else {
                Value::String(
                    if f.is_nan() {
                        "NaN"
                    } else if f.is_sign_positive() {
                        "Infinity"
                    } else {
                        "-Infinity"
                    }
                    .into(),
                )
            });
        }
        Vr::Fd if chunk.len() >= 8 => f64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]),
        _ => return None,
    };
    Some(if value.is_finite() {
        Value::Number(Number::from_f64(value).expect("finite float"))
    } else {
        Value::String(
            if value.is_nan() {
                "NaN"
            } else if value.is_sign_positive() {
                "Infinity"
            } else {
                "-Infinity"
            }
            .into(),
        )
    })
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

/// IS/DS/SV/UV rendering (§12.1).
fn numeric_value(part: &str, mode: NumberMode) -> Value {
    let trimmed = part.trim();
    match mode {
        NumberMode::AsString => Value::String(part.to_string()),
        NumberMode::AsNumber => match trimmed.parse::<f64>() {
            // NaN and infinities always stay strings (§12.1)
            Ok(value) if value.is_finite() => integer_aware_number(trimmed, value)
                .unwrap_or_else(|| Value::String(part.to_string())),
            _ => Value::String(part.to_string()),
        },
        NumberMode::PreferablyAsNumber => {
            // write a number only when reading it back yields the same text:
            // "1.50" stays a string because a number would lose the zero
            match trimmed.parse::<f64>() {
                Ok(value) if value.is_finite() && format_number_like(value) == trimmed => {
                    integer_aware_number(trimmed, value)
                        .unwrap_or_else(|| Value::String(part.to_string()))
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

/// PN as the three-group object (§12.1).
fn person_name_value(text: &str) -> Value {
    let name = PersonName::parse(text);
    let mut object = Map::new();
    let alphabetic = name.alphabetic();
    if !alphabetic.is_empty() {
        object.insert(
            "Alphabetic".to_string(),
            Value::String(alphabetic.to_string()),
        );
    }
    if let Some(ideographic) = name.ideographic().filter(|s| !s.is_empty()) {
        object.insert(
            "Ideographic".to_string(),
            Value::String(ideographic.to_string()),
        );
    }
    if let Some(phonetic) = name.phonetic().filter(|s| !s.is_empty()) {
        object.insert("Phonetic".to_string(), Value::String(phonetic.to_string()));
    }
    Value::Object(object)
}

/// Removes `//` and `/* */` comments outside JSON strings, the way fo-dicom
/// reads with `JsonCommentHandling.Skip`. Comments become a single space so
/// removing one cannot join two tokens; the input is borrowed unchanged when it
/// contains no comment, so a large `InlineBinary` payload is not copied.
fn strip_json_comments(json: &str) -> Cow<'_, str> {
    let bytes = json.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                // skip a string, honouring backslash escapes
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        b'\\' => index += 2,
                        b'"' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
            }
            b'/' if index + 1 < bytes.len() && bytes[index + 1] == b'/' => {
                match out.as_mut() {
                    Some(text) => text.push_str(&json[copied..index]),
                    None => out = Some(json[..index].to_string()),
                }
                out.as_mut().expect("just initialized").push(' ');
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                copied = index;
            }
            b'/' if index + 1 < bytes.len() && bytes[index + 1] == b'*' => {
                match out.as_mut() {
                    Some(text) => text.push_str(&json[copied..index]),
                    None => out = Some(json[..index].to_string()),
                }
                out.as_mut().expect("just initialized").push(' ');
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
                copied = index;
            }
            _ => index += 1,
        }
    }
    match out {
        None => Cow::Borrowed(json),
        Some(mut text) => {
            text.push_str(&json[copied..]);
            Cow::Owned(text)
        }
    }
}

/// Parses JSON into a value tree, tolerating comments like fo-dicom.
fn parse_json(json: &str) -> serde_json::Result<Value> {
    serde_json::from_str(&strip_json_comments(json))
}

/// Deserializes a dataset from DICOM JSON (§12.1).
///
/// With [`JsonOptions::validate_on_read`] the result is run through
/// [`Dataset::validate`], which checks every value against its VR rules and
/// value multiplicity, recursing into sequences — the equivalent of fo-dicom's
/// `autoValidate`. Without it, reading stays tolerant (§12.1).
pub fn json_to_dataset(json: &str, options: &JsonOptions) -> Result<Dataset> {
    let value = parse_json(json)?;
    let mut dataset = value_to_dataset(&value)?;
    if options.validate_on_read {
        // Honour the global/scope validation switch: a scope that disabled
        // validation keeps reading tolerant (docs §5). `Dataset::validate` is
        // itself on-demand and not scope-aware, so gate it here.
        if radicom::validation::should_validate(true) {
            dataset.validate()?;
        }
        // like fo-dicom's autoValidate dataset, later writes validate again
        dataset.set_validation(true);
    }
    Ok(dataset)
}

/// Deserializes a DICOM JSON array into a collection of datasets (§12.1).
pub fn json_to_datasets(json: &str, options: &JsonOptions) -> Result<Vec<Dataset>> {
    let value = parse_json(json)?;
    let Value::Array(values) = value else {
        return Err(JsonError::Unsupported(
            "a DICOM JSON dataset collection must be an array",
        ));
    };

    values
        .iter()
        .map(|value| {
            let mut dataset = value_to_dataset(value)?;
            if options.validate_on_read {
                if radicom::validation::should_validate(true) {
                    dataset.validate()?;
                }
                dataset.set_validation(true);
            }
            Ok(dataset)
        })
        .collect()
}

/// Deserializes a dataset from a JSON value tree (§12.1).
///
/// Reading is tolerant: the dataset is built with write-time validation off,
/// like fo-dicom's `ReadJsonDataset` `.NotValidated()`; call
/// [`Dataset::validate`] to check it explicitly.
pub fn value_to_dataset(value: &Value) -> Result<Dataset> {
    let Value::Object(object) = value else {
        return Err(JsonError::Unsupported(
            "a DICOM JSON dataset must be an object",
        ));
    };
    let mut dataset = Dataset::new();
    // JSON without autoValidate is read-tolerant (§12.1), also matching the
    // datasets the DICOM parser produces (fo-dicom `NotValidated`).
    dataset.set_validation(false);

    for (key, attribute) in object {
        let Some(tag) = parse_tag_key(key) else {
            // unknown keys (keywords, private extensions) are skipped (§12.1)
            continue;
        };
        let item = attribute_to_item(tag, attribute)?;
        dataset.insert(item)?;
    }
    restore_private_creators(&mut dataset)?;
    Ok(dataset)
}

/// Attaches the private creator named by a `(gggg,00xx)` block slot to every
/// private element in the same dataset (fo-dicom `ReadJsonDataset`
/// `JsonDicomConverter.cs:174-187`). JSON keys carry no creator, so without this
/// `get(Tag::private(..., creator))` would miss after a round trip.
fn restore_private_creators(dataset: &mut Dataset) -> Result<()> {
    let retags: Vec<(Tag, Tag)> = dataset
        .items()
        .filter_map(|item| {
            let tag = item.tag();
            if !tag.is_private() || tag.element() & 0xFF00 == 0 || tag.private_creator().is_some() {
                return None;
            }
            let slot = Tag::new(tag.group(), tag.element() >> 8);
            let creator = dataset.try_get_str(&slot)?;
            let creator = creator.trim();
            if creator.is_empty() {
                return None;
            }
            Some((
                tag.clone(),
                Tag::private(
                    tag.group(),
                    tag.element(),
                    Arc::new(PrivateCreator::new(creator)),
                ),
            ))
        })
        .collect();
    for (old, new) in retags {
        let Some(item) = dataset.remove(&old) else {
            continue;
        };
        // a sequence item must be rebuilt because `DicomItem::with_tag` is
        // crate-private in radicom
        let item = match item {
            DicomItem::Element(element) => {
                let element = match element.value().clone() {
                    ElementValue::Str(text) => DicomElement::new_str(new, element.vr(), text),
                    ElementValue::Bytes(buffer) => {
                        DicomElement::new_bytes(new, element.vr(), buffer)
                    }
                };
                DicomItem::Element(element)
            }
            DicomItem::Sequence(sequence) => {
                let mut rebuilt = DicomSequence::new(new);
                for child in sequence.items() {
                    rebuilt.push(child.clone());
                }
                DicomItem::Sequence(rebuilt)
            }
            fragments => fragments,
        };
        dataset.insert(item)?;
    }
    Ok(())
}

fn attribute_to_item(tag: Tag, attribute: &Value) -> Result<DicomItem> {
    let Value::Object(object) = attribute else {
        return Err(JsonError::Malformed {
            tag: tag.to_json_string(),
            reason: "an attribute must be an object".to_string(),
        });
    };
    // "vr" may sit at any property position (§12.1)
    let Some(vr_code) = object.get("vr").and_then(Value::as_str) else {
        return Err(JsonError::MissingVr {
            tag: tag.to_json_string(),
        });
    };
    let vr = Vr::parse_str(vr_code).map_err(|_| JsonError::UnknownVr {
        tag: tag.to_json_string(),
        vr: vr_code.to_string(),
    })?;

    if let Some(uri) = object.get("BulkDataURI").and_then(Value::as_str) {
        // JSON-02: keep the reference as a BulkDataUri buffer; the resolver that
        // owns fetching is out of scope, so the bytes stay unloaded until asked.
        return Ok(DicomItem::Element(DicomElement::new_bytes(
            tag,
            vr,
            Arc::new(radicom::model::buffer::BulkDataUri::new(uri)),
        )));
    }

    if let Some(inline) = object.get("InlineBinary") {
        // fo-dicom also accepts the single-element array form
        let encoded = match inline {
            Value::String(text) => Some(text.as_str()),
            Value::Array(values) => values.first().and_then(Value::as_str),
            _ => None,
        };
        let Some(encoded) = encoded else {
            return Err(JsonError::Malformed {
                tag: tag.to_json_string(),
                reason: "\"InlineBinary\" must be a base64 string".to_string(),
            });
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| JsonError::Malformed {
                tag: tag.to_json_string(),
                reason: format!("bad base64: {e}"),
            })?;
        return Ok(DicomItem::Element(DicomElement::new_bytes(
            tag,
            vr,
            Arc::new(MemoryBuffer::from(bytes)),
        )));
    }

    let values: &[Value] = match object.get("Value") {
        Some(Value::Array(values)) => values,
        // an attribute without a carrier is an empty element (§12.1) — except an
        // SQ, which is an empty sequence, not a length-0 element
        None | Some(Value::Null) => {
            if vr == Vr::Sq {
                return Ok(DicomItem::Sequence(DicomSequence::new(tag)));
            }
            return Ok(DicomItem::Element(DicomElement::new_empty(tag, vr)));
        }
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
        return Ok(DicomItem::Sequence(sequence));
    }

    check_value_vr(vr, &tag)?;
    let text = values_to_text(vr, values, &tag)?;
    let element = text_element(tag, vr, text)?;
    Ok(DicomItem::Element(element))
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
        return Ok(DicomElement::new_bytes(
            tag,
            vr,
            Arc::new(MemoryBuffer::from(bytes)),
        ));
    }
    if !vr.is_string() {
        let bytes = encode_numeric_or_text(vr, &text, &tag)?;
        return Ok(DicomElement::new_bytes(
            tag,
            vr,
            Arc::new(MemoryBuffer::from(bytes)),
        ));
    }
    Ok(DicomElement::new_str(tag, vr, text))
}

/// Encodes the text form of a binary numeric VR (or PN) back to bytes.
fn encode_numeric_or_text(vr: Vr, text: &str, tag: &Tag) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    // An entirely empty value is a zero-length element.
    if text.is_empty() {
        return Ok(bytes);
    }
    for part in text.split('\\') {
        // A zero-length component inside a multi-value binary VR cannot be
        // represented as a fixed-width unit; rejecting it prevents a silent VM
        // change (dropping it) or writing a bogus zero value.
        if part.trim().is_empty() {
            return Err(JsonError::Malformed {
                tag: tag_key(tag),
                reason: format!(
                    "a zero-length component is not representable for VR {}",
                    vr.code()
                ),
            });
        }
        let malformed = |kind: &str| JsonError::Malformed {
            tag: tag_key(tag),
            reason: format!("\"{part}\" is not a valid {kind}"),
        };
        match vr {
            Vr::Us => bytes.extend_from_slice(
                &part
                    .parse::<u16>()
                    .map_err(|_| malformed("US"))?
                    .to_le_bytes(),
            ),
            Vr::Ss => bytes.extend_from_slice(
                &part
                    .parse::<i16>()
                    .map_err(|_| malformed("SS"))?
                    .to_le_bytes(),
            ),
            Vr::Ul => bytes.extend_from_slice(
                &part
                    .parse::<u32>()
                    .map_err(|_| malformed("UL"))?
                    .to_le_bytes(),
            ),
            Vr::Sl => bytes.extend_from_slice(
                &part
                    .parse::<i32>()
                    .map_err(|_| malformed("SL"))?
                    .to_le_bytes(),
            ),
            Vr::Sv => bytes.extend_from_slice(
                &part
                    .parse::<i64>()
                    .map_err(|_| malformed("SV"))?
                    .to_le_bytes(),
            ),
            Vr::Uv => bytes.extend_from_slice(
                &part
                    .parse::<u64>()
                    .map_err(|_| malformed("UV"))?
                    .to_le_bytes(),
            ),
            Vr::Fl => bytes.extend_from_slice(
                &part
                    .parse::<f32>()
                    .map_err(|_| malformed("FL"))?
                    .to_le_bytes(),
            ),
            Vr::Fd => bytes.extend_from_slice(
                &part
                    .parse::<f64>()
                    .map_err(|_| malformed("FD"))?
                    .to_le_bytes(),
            ),
            // PN keeps its UTF-8 text; the charset layer encodes for the wire
            _ => bytes.extend_from_slice(part.as_bytes()),
        }
    }
    Ok(bytes)
}

/// A `Value` carrier needs a string-backed VR; binary VRs use InlineBinary.
fn check_value_vr(vr: Vr, tag: &Tag) -> Result<()> {
    if vr.is_string()
        || matches!(
            vr,
            Vr::Us | Vr::Sl | Vr::Ss | Vr::Ul | Vr::Sv | Vr::Uv | Vr::Fl | Vr::Fd | Vr::At
        )
    {
        return Ok(());
    }
    Err(JsonError::Malformed {
        tag: tag_key(tag),
        reason: format!("VR {} must use InlineBinary, not Value", vr.code()),
    })
}

/// Joins the JSON values into the `\`-separated internal text form (§12.1).
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
        // PN groups (§12.1): missing leading groups keep their `=` placeholders
        Value::Object(object) if vr == Vr::Pn => {
            let parts: Vec<String> = ["Alphabetic", "Ideographic", "Phonetic"]
                .iter()
                .map(|key| {
                    object
                        .get(*key)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                })
                .collect();
            let mut joined = parts.join("=");
            while joined.ends_with('=') {
                joined.pop();
            }
            Ok(joined)
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

    #[test]
    fn fl_serializes_with_the_f32_shortest_decimal() {
        // fo-dicom writes a FL by its f32 shortest text; a f64 expansion would
        // yield "0.10000000149011612" for 0.1f32.
        let v = binary_number(&0.1f32.to_le_bytes(), Vr::Fl).unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), "0.1");
        // FD keeps full f64 precision.
        let v = binary_number(&(0.1f64).to_le_bytes(), Vr::Fd).unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), "0.1");
    }

    #[test]
    fn validate_on_read_respects_the_validation_scope_switch() {
        // an invalid CS value that strict reading rejects
        let json = r#"{"00080060":{"vr":"CS","Value":["lower case"]}}"#;
        let strict = JsonOptions {
            validate_on_read: true,
            ..Default::default()
        };
        assert!(json_to_dataset(json, &strict).is_err());
        // a scope that disabled validation keeps validate_on_read tolerant
        {
            let _guard = radicom::validation::ValidationScopeGuard::validation_off();
            assert!(
                json_to_dataset(json, &strict).is_ok(),
                "a disabled validation scope must keep reading tolerant"
            );
        }
    }
    use radicom::model::transfer_syntax::syntaxes;

    fn sample() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
        ds.insert_str(Tag::MODALITY, Vr::Cs, "CT");
        ds.insert_str(Tag::INSTANCE_NUMBER, Vr::Is, "42");
        ds.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
    }

    #[test]
    fn review_binary_nonfinite_floats_keep_json_values() {
        for vr in [Vr::Fl, Vr::Fd] {
            let tag = Tag::new(0x0011, 0x1010);
            let mut bytes = Vec::new();
            for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                if vr == Vr::Fl {
                    bytes.extend_from_slice(&(value as f32).to_le_bytes());
                } else {
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
            }
            let mut ds = Dataset::new();
            ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
                tag.clone(),
                vr,
                Arc::new(MemoryBuffer::from(bytes)),
            )));
            let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
            let value: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(
                value["00111010"]["Value"],
                serde_json::json!(["NaN", "Infinity", "-Infinity"])
            );
            let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
            let values = back.get::<Vec<f64>>(tag).unwrap();
            assert_eq!(values.len(), 3);
            assert!(values[0].is_nan());
            assert_eq!(values[1], f64::INFINITY);
            assert_eq!(values[2], f64::NEG_INFINITY);
        }
    }

    #[test]
    fn review_single_text_json_keeps_literal_backslashes() {
        let tag = Tag::new(0x0011, 0x1010);
        for vr in [Vr::Lt, Vr::St, Vr::Ut] {
            for element in [
                DicomElement::new_str(tag.clone(), vr, "A\\B\nC"),
                DicomElement::new_bytes(
                    tag.clone(),
                    vr,
                    Arc::new(MemoryBuffer::from(b"A\\B\nC".to_vec())),
                ),
            ] {
                let mut ds = Dataset::new();
                ds.insert_or_update(DicomItem::Element(element));
                let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
                let value: Value = serde_json::from_str(&json).unwrap();
                assert_eq!(value["00111010"]["Value"], serde_json::json!(["A\\B\nC"]));
                let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
                assert_eq!(back.get_str(&tag).unwrap(), "A\\B\nC");
                back.validate().unwrap();
            }
        }
    }

    #[test]
    fn tags_use_eight_hex_digits() {
        assert_eq!(tag_key(&Tag::PATIENT_NAME), "00100010");
        assert_eq!(tag_key(&Tag::PIXEL_DATA), "7FE00010");
    }

    #[test]
    fn a_dataset_serializes_with_vr_and_value() {
        let json = dataset_to_json(&sample(), &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        let patient = &value["00100010"];
        assert_eq!(patient["vr"], "PN");
        assert_eq!(
            patient["Value"][0]["Alphabetic"], "Doe^John",
            "PN uses the three-group form"
        );
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
        assert_eq!(back.get_str(Tag::PATIENT_NAME).unwrap(), "Doe^John");
        assert_eq!(back.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
        assert_eq!(
            back.get_str(Tag::SOP_CLASS_UID).unwrap(),
            "1.2.840.10008.5.1.4.1.1.7"
        );
        assert_eq!(back.get_str(Tag::INSTANCE_NUMBER).unwrap(), "42");
        assert_eq!(back.len(), ds.len(), "no element is lost");
    }

    #[test]
    fn person_name_groups_round_trip() {
        let mut ds = Dataset::new();
        ds.insert_str(
            Tag::PATIENT_NAME,
            Vr::Pn,
            "Yamada^Tarou=山田^太郎=やまだ^たろう",
        );
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["00100010"]["Value"][0]["Ideographic"], "山田^太郎");
        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert_eq!(
            back.get_str(Tag::PATIENT_NAME).unwrap(),
            "Yamada^Tarou=山田^太郎=やまだ^たろう"
        );
    }

    #[test]
    fn multi_value_strings_split_on_backslash() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::MODALITIES_IN_STUDY, Vr::Cs, "CT\\MR");
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["00080061"]["Value"], serde_json::json!(["CT", "MR"]));
        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert_eq!(back.get_str(Tag::MODALITIES_IN_STUDY).unwrap(), "CT\\MR");
    }

    #[test]
    fn a_byte_backed_string_keeps_its_empty_components() {
        // The byte-backed branch used to drop zero-length components, so the
        // same value exported as ["A","B"] (VM 2) instead of ["A",null,"B"]
        // (VM 3) depending on how the element happened to be stored.
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::MODALITIES_IN_STUDY,
            Vr::Cs,
            Arc::new(MemoryBuffer::from(b"A\\\\B".to_vec())),
        )));

        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["00080061"]["Value"],
            serde_json::json!(["A", null, "B"]),
            "an empty component is null, like the Str-backed path"
        );

        // and it round-trips: null imports back as an empty component
        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert_eq!(back.get_str(Tag::MODALITIES_IN_STUDY).unwrap(), "A\\\\B");
    }

    #[test]
    fn empty_elements_omit_the_value_carrier() {
        let mut ds = Dataset::new();
        ds.insert_empty(Tag::PATIENT_ID, Vr::Lo);
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["00100020"]["vr"], "LO");
        assert!(
            value["00100020"].get("Value").is_none(),
            "empty elements carry no Value"
        );

        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert!(back.get_element(Tag::PATIENT_ID).unwrap().is_empty());
    }

    #[test]
    fn binary_values_use_inline_binary() {
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            Arc::new(MemoryBuffer::from(vec![1u8, 2, 3, 4])),
        )));
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["7FE00010"]["vr"], "OB");
        assert_eq!(value["7FE00010"]["InlineBinary"], "AQIDBA==");

        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        let bytes = back.get_element(Tag::PIXEL_DATA).unwrap();
        assert_eq!(
            bytes.value().as_buffer().unwrap().data().unwrap().to_vec(),
            vec![1u8, 2, 3, 4]
        );
    }

    #[test]
    fn parsed_binary_integers_serialize_as_numbers() {
        // a US element parsed from the wire is held as bytes
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::ROWS,
            Vr::Us,
            Arc::new(MemoryBuffer::from(512u16.to_le_bytes().to_vec())),
        )));
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["00280010"]["vr"], "US");
        assert_eq!(value["00280010"]["Value"][0], 512);

        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        let element = back.get_element(Tag::ROWS).unwrap();
        assert_eq!(
            element
                .value()
                .as_buffer()
                .unwrap()
                .data()
                .unwrap()
                .as_ref(),
            512u16.to_le_bytes(),
            "a US element stays byte-backed after a round-trip"
        );
        assert_eq!(
            element.value().as_buffer().unwrap().size(),
            2,
            "one US value"
        );
    }

    #[test]
    fn at_values_are_eight_hex_digits_in_either_case() {
        // AT is a binary VR, so it is held as bytes (as the parser builds it)
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::SERIES_DESCRIPTION,
            Vr::At,
            Arc::new(MemoryBuffer::from(vec![0x10, 0x00, 0x10, 0x00])),
        )));
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["0008103E"]["Value"][0], "00100010");

        let lower = r#"{"0008103E":{"vr":"AT","Value":["00100010"]}}"#;
        let back = json_to_dataset(lower, &JsonOptions::default()).unwrap();
        let element = back.get_element(Tag::SERIES_DESCRIPTION).unwrap();
        let bytes = element.value().as_buffer().unwrap();
        assert_eq!(
            bytes.data().unwrap().as_ref(),
            &[0x10, 0x00, 0x10, 0x00],
            "AT round-trips as 4-byte pairs"
        );
    }

    #[test]
    fn sequences_recurse() {
        let mut ds = Dataset::new();
        let mut item = Dataset::new();
        item.insert_str(Tag::CODE_VALUE, Vr::Sh, "113100");
        ds.insert_sequence(Tag::CONTENT_SEQUENCE).push(item);

        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["0040A730"]["vr"], "SQ");
        assert_eq!(
            value["0040A730"]["Value"][0]["00080100"]["Value"][0],
            "113100"
        );

        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        let sequence = back.get_sequence(Tag::CONTENT_SEQUENCE).unwrap();
        assert_eq!(sequence.items().len(), 1);
        assert_eq!(
            sequence.items()[0].get_str(Tag::CODE_VALUE).unwrap(),
            "113100"
        );
    }

    #[test]
    fn fragment_sequences_are_rejected_with_a_hint() {
        let mut ds = Dataset::new();
        ds.insert_fragments(Tag::PIXEL_DATA, Vr::Ob);
        let error = dataset_to_json(&ds, &JsonOptions::default()).unwrap_err();
        assert!(matches!(error, JsonError::Fragments));
        assert!(
            error.to_string().contains("transcode"),
            "the message suggests transcoding: {error}"
        );
    }

    #[test]
    fn numeric_modes_control_is_and_ds_rendering() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::INSTANCE_NUMBER, Vr::Is, "42");
        ds.insert_str(Tag::SLICE_THICKNESS, Vr::Ds, "1.50");

        let as_number = JsonOptions {
            numbers: NumberMode::AsNumber,
            ..Default::default()
        };
        let value: Value =
            serde_json::from_str(&dataset_to_json(&ds, &as_number).unwrap()).unwrap();
        assert_eq!(value["00200013"]["Value"][0], 42, "numbers by request");
        assert_eq!(value["00180050"]["Value"][0], 1.5, "numbers by request");

        let as_string = JsonOptions {
            numbers: NumberMode::AsString,
            ..Default::default()
        };
        let value: Value =
            serde_json::from_str(&dataset_to_json(&ds, &as_string).unwrap()).unwrap();
        assert_eq!(value["00200013"]["Value"][0], "42");
        assert_eq!(
            value["00180050"]["Value"][0], "1.50",
            "the string form is preserved"
        );

        // the default keeps DS trailing zeros, but writes plain integers as numbers
        let value: Value =
            serde_json::from_str(&dataset_to_json(&ds, &JsonOptions::default()).unwrap()).unwrap();
        assert_eq!(value["00200013"]["Value"][0], 42);
        assert_eq!(
            value["00180050"]["Value"][0], "1.50",
            "1.50 would lose its trailing zero"
        );
    }

    #[test]
    fn nan_and_infinity_stay_strings() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SLICE_THICKNESS, Vr::Ds, "NaN");
        let options = JsonOptions {
            numbers: NumberMode::AsNumber,
            ..Default::default()
        };
        let value: Value = serde_json::from_str(&dataset_to_json(&ds, &options).unwrap()).unwrap();
        assert_eq!(
            value["00180050"]["Value"][0], "NaN",
            "NaN can never be a JSON number"
        );
    }

    #[test]
    fn reading_accepts_numbers_for_numeric_vrs() {
        let json = r#"{"00200013":{"vr":"IS","Value":[42]}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        assert_eq!(ds.get_str(Tag::INSTANCE_NUMBER).unwrap(), "42");
    }

    #[test]
    fn reading_tolerates_unknown_properties_and_unordered_vr() {
        // "vr" after the value, plus an extra property that must be ignored
        let json = r#"{"00100020":{"Value":["PID-1"],"extra":true,"vr":"LO"}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
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
        assert!(matches!(
            json_to_dataset(json, &JsonOptions::default()),
            Err(JsonError::UnknownVr { .. })
        ));
    }

    #[test]
    fn indented_output_is_readable() {
        let options = JsonOptions {
            format_indented: true,
            ..Default::default()
        };
        let json = dataset_to_json(&sample(), &options).unwrap();
        assert!(json.contains('\n'), "pretty printing inserts newlines");
        assert!(json_to_dataset(&json, &JsonOptions::default()).is_ok());
    }

    #[test]
    fn a_non_object_root_is_rejected() {
        assert!(json_to_dataset("[1,2,3]", &JsonOptions::default()).is_err());
    }

    #[test]
    fn bulk_data_uri_round_trips() {
        // read: the URI becomes a BulkDataUri buffer, not an error (JSON-02)
        let json = r#"{"7FE00010":{"vr":"OB","BulkDataURI":"http://example/x"}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        let element = ds.get_element(Tag::PIXEL_DATA).unwrap();
        let buffer = element.value().as_buffer().unwrap();
        assert_eq!(buffer.bulk_uri(), Some("http://example/x"));

        // write: the buffer emits the URI back, not an InlineBinary
        let out = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["7FE00010"]["BulkDataURI"], "http://example/x");
        assert!(
            value["7FE00010"].get("InlineBinary").is_none(),
            "no bytes were materialized"
        );
    }

    /// A deferred buffer whose bytes are never read: `data()` panics, so a write
    /// that materialises it fails loudly instead of passing silently.
    #[derive(Debug)]
    struct UnreadableDeferred(usize);

    impl radicom::model::buffer::ByteBuffer for UnreadableDeferred {
        fn size(&self) -> u64 {
            self.0 as u64
        }

        fn is_memory(&self) -> bool {
            false
        }

        fn data(&self) -> std::io::Result<std::borrow::Cow<'_, [u8]>> {
            panic!("a deferred value was materialised");
        }
    }

    /// A deferred buffer that can produce its bytes, for the fallback cases.
    #[derive(Debug)]
    struct DeferredBytes(Vec<u8>);

    impl radicom::model::buffer::ByteBuffer for DeferredBytes {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }

        fn is_memory(&self) -> bool {
            false
        }

        fn data(&self) -> std::io::Result<std::borrow::Cow<'_, [u8]>> {
            Ok(std::borrow::Cow::Borrowed(&self.0))
        }
    }

    #[test]
    fn a_deferred_value_is_written_as_a_bulk_data_uri_without_materialising_it() {
        use radicom::model::buffer::{BulkDataUri, ByteBuffer as _};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let window: SharedBuffer = Arc::new(UnreadableDeferred(4));
        assert!(!window.is_memory(), "the fixture is a deferred value");

        let mut dataset = Dataset::new();
        dataset.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            window,
        )));

        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let options = JsonOptions {
            bulk_data_uri: Some(Arc::new(move |tag: &Tag, buffer: &SharedBuffer| {
                counter.fetch_add(1, Ordering::SeqCst);
                (!buffer.is_memory())
                    .then(|| format!("file:///bulk/{}/{}", tag.to_json_string(), buffer.size()))
            })),
            ..Default::default()
        };
        let json = dataset_to_json(&dataset, &options).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["7FE00010"]["BulkDataURI"], "file:///bulk/7FE00010/4");
        assert!(
            value["7FE00010"].get("InlineBinary").is_none(),
            "the deferred value was never base64-encoded"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one provider call");

        // an in-memory value is not offered to the provider
        let mut memory = Dataset::new();
        memory.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            Arc::new(MemoryBuffer::from(vec![1u8, 2, 3, 4])),
        )));
        let json = dataset_to_json(&memory, &options).unwrap();
        assert!(json.contains("InlineBinary"));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no call for memory");

        // a provider that declines (or names a blank URI) falls back to the bytes
        let mut fallback = Dataset::new();
        fallback.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            Arc::new(DeferredBytes(vec![1u8, 2, 3, 4])),
        )));
        let declining = JsonOptions {
            bulk_data_uri: Some(Arc::new(|_: &Tag, _: &SharedBuffer| None)),
            ..Default::default()
        };
        let json = dataset_to_json(&fallback, &declining).unwrap();
        assert!(
            json.contains(r#""InlineBinary":"AQIDBA==""#),
            "no URI means InlineBinary, got {json}"
        );

        let blank = JsonOptions {
            bulk_data_uri: Some(Arc::new(|_: &Tag, _: &SharedBuffer| Some("  ".into()))),
            ..Default::default()
        };
        let json = dataset_to_json(&fallback, &blank).unwrap();
        assert!(
            json.contains(r#""InlineBinary":"AQIDBA==""#) && !json.contains("BulkDataURI"),
            "a blank URI is not a carrier, got {json}"
        );

        // the URI comes back as a deferred buffer, and resolving it yields the
        // original bytes — the round trip a resolver performs
        let parsed = json_to_dataset(
            &serde_json::json!({"7FE00010": {"vr": "OB", "BulkDataURI": "file:///bulk/7FE00010/4"}})
                .to_string(),
            &JsonOptions::default(),
        )
        .unwrap();
        let element = parsed.get_element(Tag::PIXEL_DATA).unwrap();
        let deferred = element.value().as_buffer().unwrap();
        assert_eq!(deferred.bulk_uri(), Some("file:///bulk/7FE00010/4"));
        assert!(!deferred.is_memory(), "still unresolved");
        let resolved = BulkDataUri::with_data("file:///bulk/7FE00010/4", vec![1u8, 2, 3, 4]);
        assert_eq!(resolved.data().unwrap().as_ref(), &[1u8, 2, 3, 4]);
        assert_eq!(resolved.bulk_uri(), Some("file:///bulk/7FE00010/4"));
    }

    #[test]
    fn duplicate_properties_take_the_last_value() {
        // serde_json itself keeps the last duplicate, which is the rule (§12.1)
        let json = r#"{"00100020":{"vr":"LO","Value":["first"]},"00100020":{"vr":"LO","Value":["second"]}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "second");
    }

    #[test]
    fn group_lengths_are_stripped_at_every_level() {
        // PS3.18 Annex F has no form for (GGGG,0000), so it is never written —
        // neither at the top level nor inside a sequence item (JSON-01). The
        // expected string is written out by hand, not generated by the encoder.
        let mut ds = Dataset::new();
        ds.insert_value(Tag::new(0x0008, 0x0000), 208u32);
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        let mut item = Dataset::new();
        item.insert_value(Tag::new(0x0008, 0x0000), 24u32);
        item.insert_str(Tag::CODE_VALUE, Vr::Sh, "113100");
        ds.insert_sequence(Tag::CONTENT_SEQUENCE).push(item);

        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(
            json,
            concat!(
                r#"{"00080016":{"Value":["1.2.840.10008.5.1.4.1.1.7"],"vr":"UI"},"#,
                r#""0040A730":{"Value":[{"00080100":{"Value":["113100"],"vr":"SH"}}],"vr":"SQ"}}"#
            )
        );
    }

    #[test]
    fn validate_on_read_checks_values_against_their_vr() {
        // CS allows upper case only; this value is not valid CS
        let json = r#"{"00080060":{"vr":"CS","Value":["lower case"]}}"#;
        // reading is tolerant by default (§12.1)
        assert!(json_to_dataset(json, &JsonOptions::default()).is_ok());

        let strict = JsonOptions {
            validate_on_read: true,
            ..Default::default()
        };
        match json_to_dataset(json, &strict) {
            Err(JsonError::Dataset(error)) => {
                assert!(matches!(error, radicom::Error::Validation(_)), "{error}");
                assert!(
                    error.to_string().contains("0008,0060"),
                    "the failure names the tag: {error}"
                );
            }
            other => panic!("expected a validation failure, got {other:?}"),
        }

        // a valid dataset passes, but an invalid value nested in an item fails
        assert!(json_to_dataset(r#"{"00080060":{"vr":"CS","Value":["CT"]}}"#, &strict).is_ok());
        let nested =
            r#"{"0040A730":{"vr":"SQ","Value":[{"00080060":{"vr":"CS","Value":["lower case"]}}]}}"#;
        assert!(
            json_to_dataset(nested, &strict).is_err(),
            "validation recurses into items"
        );
    }

    #[test]
    fn an_empty_sequence_omits_its_value() {
        // PS3.18 Annex F / fo-dicom: no items → no "Value" array
        let mut ds = Dataset::new();
        ds.insert_sequence(Tag::CONTENT_SEQUENCE);
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(json, r#"{"0040A730":{"vr":"SQ"}}"#);

        // and the bare form reads back as an empty sequence, not an SQ element
        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        let sequence = back
            .get_sequence(Tag::CONTENT_SEQUENCE)
            .expect("an empty SQ is a sequence");
        assert!(sequence.items().is_empty());
    }

    #[test]
    fn an_empty_value_component_is_null() {
        // PS3.18 Annex F: a zero-length value component is null, not ""
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SPECIFIC_CHARACTER_SET, Vr::Cs, "\\ISO 2022 IR 58");
        let json = dataset_to_json(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(
            json,
            r#"{"00080005":{"Value":[null,"ISO 2022 IR 58"],"vr":"CS"}}"#
        );

        // reading the null back restores the empty first component
        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert_eq!(
            back.get_str(Tag::SPECIFIC_CHARACTER_SET).unwrap(),
            "\\ISO 2022 IR 58"
        );
    }

    #[test]
    fn is_values_are_numbers_only_when_that_is_lossless() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::INSTANCE_NUMBER, Vr::Is, "0001");

        // the default keeps "0001": writing 1 back would change the text, so the
        // string is the lossless form (fo-dicom's AsNumber default writes 1)
        assert_eq!(
            dataset_to_json(&ds, &JsonOptions::default()).unwrap(),
            r#"{"00200013":{"Value":["0001"],"vr":"IS"}}"#
        );

        let as_number = JsonOptions {
            numbers: NumberMode::AsNumber,
            ..Default::default()
        };
        assert_eq!(
            dataset_to_json(&ds, &as_number).unwrap(),
            r#"{"00200013":{"Value":[1],"vr":"IS"}}"#
        );

        let as_string = JsonOptions {
            numbers: NumberMode::AsString,
            ..Default::default()
        };
        assert_eq!(
            dataset_to_json(&ds, &as_string).unwrap(),
            r#"{"00200013":{"Value":["0001"],"vr":"IS"}}"#
        );
    }

    #[test]
    fn dataset_collections_round_trip_as_a_json_array() {
        let mut second = sample();
        second.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-2");
        let datasets = vec![sample(), second];

        let json = datasets_to_json(&datasets, &JsonOptions::default()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert!(value.is_array());
        assert_eq!(value.as_array().unwrap().len(), 2);

        let decoded = json_to_datasets(&json, &JsonOptions::default()).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
        assert_eq!(decoded[1].get_str(Tag::PATIENT_ID).unwrap(), "PID-2");
    }

    #[test]
    fn empty_dataset_collection_is_an_empty_array() {
        let json = datasets_to_json(&[], &JsonOptions::default()).unwrap();
        assert_eq!(json, "[]");
        assert!(json_to_datasets(&json, &JsonOptions::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn dataset_collection_preserves_nested_sequences() {
        let mut dataset = Dataset::new();
        let mut item = Dataset::new();
        item.insert_str(Tag::CODE_VALUE, Vr::Sh, "113100");
        dataset.insert_sequence(Tag::CONTENT_SEQUENCE).push(item);

        let json = datasets_to_json(&[dataset], &JsonOptions::default()).unwrap();
        let decoded = json_to_datasets(&json, &JsonOptions::default()).unwrap();
        assert_eq!(
            decoded[0]
                .get_sequence(Tag::CONTENT_SEQUENCE)
                .unwrap()
                .items()[0]
                .get_str(Tag::CODE_VALUE)
                .unwrap(),
            "113100"
        );
    }

    #[test]
    fn dataset_collection_requires_a_json_array_root() {
        for json in ["{}", "null", "1"] {
            assert!(matches!(
                json_to_datasets(json, &JsonOptions::default()),
                Err(JsonError::Unsupported(
                    "a DICOM JSON dataset collection must be an array"
                ))
            ));
        }
    }

    #[test]
    fn dataset_collection_propagates_entry_errors_and_validation() {
        let malformed = r#"[{"00100020":{"Value":["PID-1"]}}]"#;
        assert!(matches!(
            json_to_datasets(malformed, &JsonOptions::default()),
            Err(JsonError::MissingVr { .. })
        ));

        let invalid = r#"[{"00080060":{"vr":"CS","Value":["lower case"]}}]"#;
        let options = JsonOptions {
            validate_on_read: true,
            ..Default::default()
        };
        assert!(matches!(
            json_to_datasets(invalid, &options),
            Err(JsonError::Dataset(_))
        ));
    }

    #[test]
    fn imported_person_name_survives_a_dicom_file_round_trip() {
        // review #9: PN must be a normal string element so the writer does not
        // emit an empty value for a byte-backed element.
        let json = r#"{"00100010":{"vr":"PN","Value":[{"Alphabetic":"Doe^Jane"}]}}"#;
        let mut ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        let bytes = radicom::io::file::DicomFile::from_dataset(ds)
            .to_bytes(&radicom::io::writer::WriteOptions::default())
            .unwrap();
        let back = radicom::io::file::DicomFile::from_bytes(
            &bytes,
            &radicom::io::file::ReadOptions::default(),
        )
        .unwrap();
        assert_eq!(back.dataset.get_str(Tag::PATIENT_NAME).unwrap(), "Doe^Jane");
    }

    #[test]
    fn person_name_multi_value_and_missing_groups_keep_their_shape() {
        // multi-value export splits on '\'
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^Jane\\Roe^John");
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        let names = value["00100010"]["Value"].as_array().unwrap();
        assert_eq!(names.len(), 2, "one object per PN value");
        assert_eq!(names[1]["Alphabetic"], "Roe^John");

        // a missing Alphabetic group keeps its leading '=' placeholder
        let json = r#"{"00100010":{"vr":"PN","Value":[{"Ideographic":"山田"}]}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        assert_eq!(ds.get_str(Tag::PATIENT_NAME).unwrap(), "=山田");
    }

    #[test]
    fn signed_and_unsigned_64bit_values_round_trip_exactly() {
        // review #12: SV/UV must not degrade to null or lose 64-bit precision.
        let json = r#"{"00720010":{"vr":"SV","Value":[-1]},"00720012":{"vr":"UV","Value":[18446744073709551615]}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        let out = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(out["00720010"]["Value"][0], serde_json::json!(-1));
        assert_eq!(
            out["00720012"]["Value"][0],
            serde_json::json!(18446744073709551615u64)
        );
    }

    #[test]
    fn indented_dataset_collection_output_contains_newlines() {
        let options = JsonOptions {
            format_indented: true,
            ..Default::default()
        };
        let json = datasets_to_json(&[sample()], &options).unwrap();
        assert!(json.contains('\n'));
    }

    #[test]
    fn keyword_keys_fall_back_to_hex_for_unknown_tags() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "P1");
        // a private dictionary tag whose keyword is empty must fall back to hex
        let creator = Arc::new(radicom::model::private_creator::PrivateCreator::new(
            "MED NM",
        ));
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::private(0x0011, 0x1010, creator),
            Vr::Un,
            Arc::new(MemoryBuffer::from(b"no-keyword".to_vec())),
        )));
        let options = JsonOptions {
            tags_as_keywords: true,
            ..Default::default()
        };
        let value = dataset_to_value(&ds, &options).unwrap();
        assert!(value.get("PatientID").is_some(), "{value}");
        assert!(
            value.get("00111010").is_some(),
            "a tag with no keyword keeps its hex key: {value}"
        );
    }

    #[test]
    fn a_length_zero_sq_element_serializes_bare_and_reads_back_empty() {
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_empty(
            Tag::REFERENCED_IMAGE_SEQUENCE,
            Vr::Sq,
        )));
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(value["00081140"]["vr"], "SQ");
        assert!(value["00081140"].get("Value").is_none());
        let json = serde_json::to_string(&value).unwrap();
        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert_eq!(
            back.try_get_sequence(Tag::REFERENCED_IMAGE_SEQUENCE)
                .unwrap()
                .items()
                .len(),
            0
        );
    }

    #[test]
    fn an_empty_binary_element_has_no_inline_binary() {
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            Arc::new(MemoryBuffer::from(Vec::new())),
        )));
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(value["7FE00010"]["vr"], "OB");
        assert!(value["7FE00010"].get("InlineBinary").is_none());
    }

    #[test]
    fn at_values_are_upper_case_hex_and_round_trip() {
        // reading a lower-case AT value normalizes it to the 8-hex form
        let json = r#"{"00080052":{"vr":"AT","Value":["00100010","7fe00010"]}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(
            value["00080052"]["Value"],
            serde_json::json!(["00100010", "7FE00010"])
        );
        // and the bytes are the big-endian pair order of the tag
        let element = ds.get_element(Tag::QUERY_RETRIEVE_LEVEL).unwrap();
        assert_eq!(
            element
                .value()
                .as_buffer()
                .unwrap()
                .data()
                .unwrap()
                .as_ref(),
            &[0x10, 0x00, 0x10, 0x00, 0xE0, 0x7F, 0x10, 0x00]
        );
    }

    #[test]
    fn numeric_binary_vrs_round_trip_through_value() {
        let json = r#"{
            "00110001":{"vr":"US","Value":[65535]},
            "00110002":{"vr":"SS","Value":[-2]},
            "00110003":{"vr":"UL","Value":[4294967295]},
            "00110004":{"vr":"SL","Value":[-70000]},
            "00110005":{"vr":"SV","Value":[-5]},
            "00110006":{"vr":"UV","Value":[9]},
            "00110007":{"vr":"FL","Value":[1.5]},
            "00110008":{"vr":"FD","Value":[2.25]}
        }"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        let out = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(out["00110001"]["Value"][0], serde_json::json!(65535));
        assert_eq!(out["00110002"]["Value"][0], serde_json::json!(-2));
        assert_eq!(
            out["00110003"]["Value"][0],
            serde_json::json!(4294967295u32)
        );
        assert_eq!(out["00110004"]["Value"][0], serde_json::json!(-70000));
        assert_eq!(out["00110005"]["Value"][0], serde_json::json!(-5));
        assert_eq!(out["00110006"]["Value"][0], serde_json::json!(9));
        assert_eq!(out["00110007"]["Value"][0], serde_json::json!(1.5));
        assert_eq!(out["00110008"]["Value"][0], serde_json::json!(2.25));
    }

    #[test]
    fn number_modes_control_is_ds_rendering() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::INSTANCE_NUMBER, Vr::Is, "42");
        ds.insert_str(Tag::SLICE_THICKNESS, Vr::Ds, "1.50");

        let as_string = JsonOptions {
            numbers: NumberMode::AsString,
            ..Default::default()
        };
        let value = dataset_to_value(&ds, &as_string).unwrap();
        assert_eq!(value["00200013"]["Value"][0], serde_json::json!("42"));

        let as_number = JsonOptions {
            numbers: NumberMode::AsNumber,
            ..Default::default()
        };
        let value = dataset_to_value(&ds, &as_number).unwrap();
        assert_eq!(value["00180050"]["Value"][0], serde_json::json!(1.5));

        // the default keeps "1.50" as a string because a number would lose the zero
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(value["00180050"]["Value"][0], serde_json::json!("1.50"));
        assert_eq!(value["00200013"]["Value"][0], serde_json::json!(42));
    }

    #[test]
    fn malformed_attributes_are_typed_errors() {
        type Case = (&'static str, fn(&JsonError) -> bool);
        let cases: [Case; 10] = [
            (r#"{"00100020":"not an object"}"#, |e| {
                matches!(e, JsonError::Malformed { .. })
            }),
            (r#"{"00100020":{"Value":["x"]}}"#, |e| {
                matches!(e, JsonError::MissingVr { .. })
            }),
            (r#"{"00100020":{"vr":"ZZ","Value":["x"]}}"#, |e| {
                matches!(e, JsonError::UnknownVr { .. })
            }),
            (r#"{"00100020":{"vr":"LO","Value":"x"}}"#, |e| {
                matches!(e, JsonError::Malformed { .. })
            }),
            (r#"{"7FE00010":{"vr":"OB","InlineBinary":"!!"}}"#, |e| {
                matches!(e, JsonError::Malformed { .. })
            }),
            (r#"{"7FE00010":{"vr":"OB","Value":["x"]}}"#, |e| {
                matches!(e, JsonError::Malformed { .. })
            }),
            (r#"{"00080052":{"vr":"AT","Value":["nope"]}}"#, |e| {
                matches!(e, JsonError::Malformed { .. })
            }),
            (
                r#"{"00100010":{"vr":"LO","Value":[{"Alphabetic":"x"}]}}"#,
                |e| matches!(e, JsonError::Malformed { .. }),
            ),
            (r#"{"00100020":{"vr":"LO","Value":[["x"]]}}"#, |e| {
                matches!(e, JsonError::Malformed { .. })
            }),
            (
                r#"{"00280010":{"vr":"US","Value":["abc"]}}"#,
                |e| matches!(e, JsonError::Malformed { tag, .. } if !tag.is_empty()),
            ),
        ];
        for (json, predicate) in cases {
            let err = json_to_dataset(json, &JsonOptions::default()).expect_err(json);
            assert!(predicate(&err), "{json} produced {err:?}");
        }

        // a boolean is not a DICOM value
        assert!(matches!(
            json_to_dataset(
                r#"{"00100020":{"vr":"LO","Value":[true]}}"#,
                &JsonOptions::default()
            ),
            Err(JsonError::Malformed { .. })
        ));
        // a JSON array alone is not a dataset
        assert!(matches!(
            value_to_dataset(&serde_json::json!([])),
            Err(JsonError::Unsupported(_))
        ));
        assert!(matches!(
            json_to_datasets("{}", &JsonOptions::default()),
            Err(JsonError::Unsupported(_))
        ));
    }

    #[test]
    fn an_empty_value_array_reads_as_an_empty_element() {
        let json = r#"{"00100020":{"vr":"LO","Value":[]}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        assert!(ds.get_element(Tag::PATIENT_ID).unwrap().is_empty());
    }

    #[test]
    fn a_phonetic_only_person_name_round_trips() {
        let json = r#"{"00100010":{"vr":"PN","Value":[{"Phonetic":"Yamada"}]}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        assert_eq!(ds.get_str(Tag::PATIENT_NAME).unwrap(), "==Yamada");
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(value["00100010"]["Value"][0]["Phonetic"], "Yamada");
    }

    #[test]
    fn a_non_array_value_carrier_names_the_offending_json_type() {
        for (value, expected) in [
            ("true", "a boolean"),
            ("1", "a number"),
            ("\"x\"", "a string"),
            ("{}", "an object"),
        ] {
            let json = format!(r#"{{"00100020":{{"vr":"LO","Value":{value}}}}}"#);
            let error = json_to_dataset(&json, &JsonOptions::default()).expect_err(&json);
            match error {
                JsonError::Malformed { reason, .. } => {
                    assert!(
                        reason.contains(expected),
                        "{json}: expected {expected} in {reason}"
                    );
                }
                other => panic!("{json} produced {other:?}"),
            }
        }
    }

    #[test]
    fn numeric_text_helpers_keep_unparseable_values_as_strings() {
        assert_eq!(parse_int_value("abc", Vr::Us), Value::String("abc".into()));
        assert_eq!(parse_int_value("-3", Vr::Ss), serde_json::json!(-3));
        assert_eq!(
            parse_int_value("99999999999999999999999", Vr::Ul),
            Value::String("99999999999999999999999".into())
        );
        assert_eq!(parse_float_value("1.5"), serde_json::json!(1.5));
        assert_eq!(parse_float_value("x"), Value::String("x".into()));
        assert_eq!(type_name(&Value::Null), "null");
        assert_eq!(type_name(&Value::Bool(true)), "a boolean");
        assert_eq!(type_name(&Value::Number(1.into())), "a number");
        assert_eq!(type_name(&Value::String(String::new())), "a string");
        assert_eq!(type_name(&Value::Array(Vec::new())), "an array");
        assert_eq!(type_name(&Value::Object(Map::new())), "an object");
    }

    #[test]
    fn binary_number_covers_float_and_unknown_vrs() {
        assert_eq!(
            binary_number(&1.0f32.to_le_bytes(), Vr::Fl),
            Some(serde_json::json!(1.0))
        );
        assert_eq!(
            binary_number(&1.5f64.to_le_bytes(), Vr::Fd),
            Some(serde_json::json!(1.5))
        );
        assert_eq!(binary_number(&[1, 2], Vr::Un), None);
        assert_eq!(binary_number(&[], Vr::Us), None);
    }

    #[test]
    fn a_u64_sized_integer_is_rendered_exactly_in_as_number_mode() {
        // `integer_aware_number` must take the u64 path for values above
        // i64::MAX instead of rounding through f64.
        let value = numeric_value("18446744073709551615", NumberMode::AsNumber);
        assert_eq!(value, serde_json::json!(18446744073709551615u64));
        // and it never fabricates a number for a non-finite or unparseable text
        assert_eq!(
            numeric_value("NaN", NumberMode::AsNumber),
            Value::String("NaN".into())
        );
    }

    #[test]
    fn empty_binary_numeric_components_are_rejected_not_silently_dropped() {
        // A US value cannot hold a zero-length component; dropping it silently
        // would change the VM, so the read is rejected.
        let json = r#"{"00280010":{"vr":"US","Value":["","5"]}}"#;
        assert!(matches!(
            json_to_dataset(json, &JsonOptions::default()),
            Err(JsonError::Malformed { .. })
        ));
        // a genuinely empty Value array is still an empty element
        let empty = json_to_dataset(
            r#"{"00280010":{"vr":"US","Value":[]}}"#,
            &JsonOptions::default(),
        )
        .unwrap();
        assert!(empty.get_element(Tag::ROWS).unwrap().is_empty());
    }

    #[test]
    fn numeric_empty_components_serialize_as_null_and_round_trip() {
        // FJ-03: a trailing empty DS component is null in JSON, not "", and
        // survives a write/read/write round trip.
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SLICE_THICKNESS, Vr::Ds, "1.50\\");
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(
            value["00180050"]["Value"],
            serde_json::json!(["1.50", null]),
            "the empty component is null, not \"\""
        );

        let json = serde_json::to_string(&value).unwrap();
        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert_eq!(
            back.get_str(Tag::SLICE_THICKNESS).unwrap(),
            "1.50\\",
            "the empty trailing component is preserved"
        );
        let again = dataset_to_value(&back, &JsonOptions::default()).unwrap();
        assert_eq!(
            again["00180050"]["Value"],
            serde_json::json!(["1.50", null])
        );
    }

    #[test]
    fn a_non_binary_bulk_data_uri_survives_a_round_trip() {
        // FJ-D-02: BulkDataURI is legal for DS/IS/... too (PS3.18 F.2.2); the
        // reader keeps it as a BulkDataUri buffer and the writer must re-emit
        // it instead of dropping it as an empty value.
        let json = r#"{"00180050":{"vr":"DS","BulkDataURI":"http://example/ds"}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(
            value["00180050"]["BulkDataURI"], "http://example/ds",
            "the DS URI survives"
        );
        assert!(
            value["00180050"].get("Value").is_none(),
            "a URI carrier has no Value"
        );

        // the binary case keeps working too
        let ob = r#"{"7FE00010":{"vr":"OB","BulkDataURI":"http://example/ob"}}"#;
        let ds = json_to_dataset(ob, &JsonOptions::default()).unwrap();
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(value["7FE00010"]["BulkDataURI"], "http://example/ob");
    }

    #[test]
    fn an_empty_person_name_component_is_null() {
        // FJ-D-04: a trailing empty PN component is null, matching the string
        // and numeric paths, instead of an empty `{}` object.
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John\\");
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(
            value["00100010"]["Value"],
            serde_json::json!([{"Alphabetic": "Doe^John"}, null])
        );

        // the byte backend takes the same route
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PATIENT_NAME,
            Vr::Pn,
            Arc::new(MemoryBuffer::from(b"Doe^John\\".to_vec())),
        )));
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(
            value["00100010"]["Value"],
            serde_json::json!([{"Alphabetic": "Doe^John"}, null])
        );
    }

    #[test]
    fn a_byte_backed_ds_honors_number_mode() {
        // FJ-D-05: the byte backend dropped JsonOptions::numbers and always
        // wrote a string.
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::SLICE_THICKNESS,
            Vr::Ds,
            Arc::new(MemoryBuffer::from(b"42".to_vec())),
        )));

        let as_number = JsonOptions {
            numbers: NumberMode::AsNumber,
            ..Default::default()
        };
        let value = dataset_to_value(&ds, &as_number).unwrap();
        assert_eq!(value["00180050"]["Value"][0], serde_json::json!(42));

        let as_string = JsonOptions {
            numbers: NumberMode::AsString,
            ..Default::default()
        };
        let value = dataset_to_value(&ds, &as_string).unwrap();
        assert_eq!(value["00180050"]["Value"][0], serde_json::json!("42"));
    }

    #[test]
    fn json_export_rejects_truncated_binary_numeric_values() {
        for vr in [
            Vr::Us,
            Vr::Ss,
            Vr::Ul,
            Vr::Sl,
            Vr::Fl,
            Vr::Fd,
            Vr::Sv,
            Vr::Uv,
            Vr::At,
        ] {
            let tag = Tag::new(0x7776, 0x0010);
            let mut ds = Dataset::new();
            ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
                tag.clone(),
                vr,
                Arc::new(MemoryBuffer::from(vec![0; vr.unit_size() + 1])),
            )));
            match dataset_to_json(&ds, &JsonOptions::default()) {
                Err(JsonError::Malformed {
                    tag: actual,
                    reason,
                }) => {
                    assert_eq!(actual, tag.to_string());
                    assert!(reason.contains(&vr.to_string()), "{reason}");
                    assert!(reason.contains("not a multiple"), "{reason}");
                }
                result => panic!("{vr} with a truncated final value must fail: {result:?}"),
            }
        }
    }

    #[test]
    fn binary_numeric_json_from_this_library_reads_back() {
        // FJ-D-06: the writer never emits `null` for a fixed-width binary
        // numeric VR (they are byte-backed), so its own output is readable.
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::ROWS,
            Vr::Us,
            Arc::new(MemoryBuffer::from(vec![5u8, 0, 7u8, 0])),
        )));
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert_eq!(value["00280010"]["Value"], serde_json::json!([5, 7]));
        let json = serde_json::to_string(&value).unwrap();
        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert_eq!(back.get::<Vec<u16>>(Tag::ROWS).unwrap(), vec![5, 7]);
    }

    #[test]
    fn byte_backed_person_name_uses_the_object_form() {
        // FJ-04: a PN held as bytes must still serialize as a PN object
        // (Alphabetic/Ideographic/Phonetic), never as a plain string.
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PATIENT_NAME,
            Vr::Pn,
            Arc::new(MemoryBuffer::from(b"Doe^Jane=Yamada^Tarou".to_vec())),
        )));
        let value = dataset_to_value(&ds, &JsonOptions::default()).unwrap();
        assert!(
            value["00100010"]["Value"][0].is_object(),
            "PN must be an object, got {}",
            value["00100010"]["Value"][0]
        );
        assert_eq!(value["00100010"]["Value"][0]["Alphabetic"], "Doe^Jane");
        assert_eq!(value["00100010"]["Value"][0]["Ideographic"], "Yamada^Tarou");
    }

    /// A dataset whose tags would all collapse onto one keyword: unknown tags
    /// (`Unknown`), repeating-group overlay tags (`OverlayData`) and a private
    /// element behind a creator slot.
    fn collision_prone_dataset() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
        ds.insert_str(Tag::new(0x7777, 0x0001), Vr::Lo, "unknown-1");
        ds.insert_str(Tag::new(0x7777, 0x0002), Vr::Lo, "unknown-2");
        ds.insert_value(Tag::new(0x6000, 0x0010), 2u16);
        ds.insert_value(Tag::new(0x6002, 0x0010), 3u16);
        ds.insert_str(Tag::new(0x0011, 0x0010), Vr::Lo, "ACME 1.2");
        ds.insert_str(
            Tag::private(0x0011, 0x1010, Arc::new(PrivateCreator::new("ACME 1.2"))),
            Vr::Lo,
            "secret",
        );
        ds
    }

    #[test]
    fn tags_as_keywords_falls_back_to_hex_and_loses_no_element() {
        let ds = collision_prone_dataset();
        let options = JsonOptions {
            tags_as_keywords: true,
            ..Default::default()
        };
        let value = dataset_to_value(&ds, &options).unwrap();
        let object = value.as_object().expect("dataset object");
        assert_eq!(
            object.len(),
            ds.len(),
            "every element needs its own key: {value}"
        );
        for key in [
            "77770001", "77770002", "60000010", "60020010", "00110010", "00111010",
        ] {
            assert!(
                object.contains_key(key),
                "hex key {key} is present: {value}"
            );
        }
        assert!(object.contains_key("PatientName"));
        assert!(!object.contains_key("Unknown"));
        assert!(!object.contains_key("OverlayRows"));
        assert!(!object.contains_key("PrivateCreator"));
    }

    #[test]
    fn keyword_keyed_json_round_trips_and_unknown_keys_stay_skipped() {
        let ds = collision_prone_dataset();
        let options = JsonOptions {
            tags_as_keywords: true,
            ..Default::default()
        };
        let json = dataset_to_json(&ds, &options).unwrap();
        assert!(json.contains("\"PatientName\""), "{json}");

        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert_eq!(
            dataset_to_json(&back, &JsonOptions::default()).unwrap(),
            dataset_to_json(&ds, &JsonOptions::default()).unwrap(),
            "keyword-mode output reads back as the original dataset"
        );

        // a key that is neither a tag nor a keyword is still skipped
        let ds = json_to_dataset(
            r#"{"NotAKeyword":{"vr":"LO","Value":["x"]},"00100020":{"vr":"LO","Value":["PID-1"]}}"#,
            &JsonOptions::default(),
        )
        .unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
    }

    #[test]
    fn private_creators_are_restored_when_reading_json() {
        let json = r#"{"00290010":{"vr":"LO","Value":["ACME 1.2"]},
                       "00291001":{"vr":"LO","Value":["secret"]}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        let tag = Tag::private(0x0029, 0x1001, Arc::new(PrivateCreator::new("ACME 1.2")));
        assert_eq!(ds.try_get_str(&tag).as_deref(), Some("secret"));
        assert!(
            !ds.contains(&Tag::new(0x0029, 0x1001)),
            "the creator-less tag is replaced by the creator-bound one"
        );

        // sequence items get the same treatment
        let nested = r#"{"0040A730":{"vr":"SQ","Value":[
            {"00110010":{"vr":"LO","Value":["ACME"]},
             "00111010":{"vr":"LO","Value":["nested"]}}]}}"#;
        let ds = json_to_dataset(nested, &JsonOptions::default()).unwrap();
        let item = &ds.try_get_sequence(Tag::CONTENT_SEQUENCE).unwrap().items()[0];
        let tag = Tag::private(0x0011, 0x1010, Arc::new(PrivateCreator::new("ACME")));
        assert_eq!(item.try_get_str(&tag).as_deref(), Some("nested"));
    }

    #[test]
    fn json_reading_accepts_comments_and_array_inline_binary() {
        let json = r#"
            // a line comment before the object
            {
                /* a block comment */ "00100020": {"vr":"LO","Value":["PID-1"]},
                "00080080": {"vr":"LO","Value":["a//b /*c*/"]},
                "7FE00010": {"vr":"OB","InlineBinary":["AAECAw=="]}
            }"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
        assert_eq!(
            ds.get_str(Tag::INSTITUTION_NAME).unwrap(),
            "a//b /*c*/",
            "comment markers inside a string are data, not comments"
        );
        assert_eq!(
            ds.get_element(Tag::PIXEL_DATA)
                .unwrap()
                .value()
                .as_buffer()
                .unwrap()
                .data()
                .unwrap()
                .as_ref(),
            &[0u8, 1, 2, 3],
            "the array form of InlineBinary is decoded, not dropped"
        );

        // malformed InlineBinary is diagnosed instead of silently dropped
        let bad = r#"{"7FE00010":{"vr":"OB","InlineBinary":[]}}"#;
        assert!(matches!(
            json_to_dataset(bad, &JsonOptions::default()),
            Err(JsonError::Malformed { .. })
        ));
    }

    #[test]
    fn keyword_and_name_annotations_are_optional_writer_extensions() {
        let options = JsonOptions {
            write_keyword: true,
            write_name: true,
            ..Default::default()
        };
        let value = dataset_to_value(&sample(), &options).unwrap();
        assert_eq!(value["00100010"]["keyword"], "PatientName");
        assert!(value["00100010"]["name"]
            .as_str()
            .unwrap()
            .contains("Patient"));
        assert!(value["00100010"].get("value").is_none());

        // unknown and masked tags carry no annotation
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x7777, 0x0001), Vr::Lo, "x");
        ds.insert_value(Tag::new(0x6000, 0x0010), 2u16);
        let value = dataset_to_value(&ds, &options).unwrap();
        assert!(value["77770001"].get("keyword").is_none());
        assert!(value["60000010"].get("keyword").is_none());

        // reading the annotation back ignores the extra properties
        let json = dataset_to_json(&sample(), &options).unwrap();
        let back = json_to_dataset(&json, &JsonOptions::default()).unwrap();
        assert_eq!(
            dataset_to_json(&back, &JsonOptions::default()).unwrap(),
            dataset_to_json(&sample(), &JsonOptions::default()).unwrap()
        );
    }

    #[test]
    fn a_bulk_data_uri_on_a_forbidden_vr_is_an_error() {
        // PS3.18 F.2.2 lists the VRs a BulkDataURI may carry; PN is not one, so
        // the value must be materialised rather than emitted as an invalid URI.
        let json = r#"{"00100010":{"vr":"PN","BulkDataURI":"http://example/pn"}}"#;
        let ds = json_to_dataset(json, &JsonOptions::default()).unwrap();
        assert!(dataset_to_value(&ds, &JsonOptions::default()).is_err());
    }
}
