//! DICOM element value model and typed access (ELEM-01, ELEM-02;
//! docs/04-data-model.md §7).

use crate::error::{DataError, Error};
use crate::model::buffer::{MemoryBuffer, SharedBuffer};
use crate::model::dicom_datetime::{parse_da, parse_dt, parse_tm, DtPoint};
use crate::model::person_name::{parse_ds, parse_is, PersonName};
use crate::model::tag::Tag;
use crate::model::transfer_syntax::TransferSyntax;
use crate::model::uid::Uid;
use crate::model::vr::Vr;
use chrono::{NaiveDate, NaiveTime};
use std::sync::Arc;

/// The payload of an element: lazily-encoded strings or materialized bytes.
///
/// String elements keep their text form; byte encoding (with charset and
/// padding) happens at serialization time (ELEM-05 lazy semantics). Binary
/// VRs hold a shared buffer.
#[derive(Debug, Clone)]
pub enum ElementValue {
    Str(String),
    Bytes(SharedBuffer),
}

impl ElementValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            ElementValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_buffer(&self) -> Option<&SharedBuffer> {
        match self {
            ElementValue::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            ElementValue::Str(s) => s.is_empty(),
            ElementValue::Bytes(b) => b.size() == 0,
        }
    }
}

/// A DICOM data element: tag + VR + value.
#[derive(Debug, Clone)]
pub struct DicomElement {
    tag: Tag,
    vr: Vr,
    value: ElementValue,
}

impl DicomElement {
    pub fn new_str(tag: Tag, vr: Vr, text: impl Into<String>) -> Self {
        debug_assert!(vr.is_string() || vr == Vr::Implicit, "{} is not a string VR", vr);
        DicomElement { tag, vr, value: ElementValue::Str(text.into()) }
    }

    pub fn new_bytes(tag: Tag, vr: Vr, buffer: SharedBuffer) -> Self {
        DicomElement { tag, vr, value: ElementValue::Bytes(buffer) }
    }

    pub fn new_empty(tag: Tag, vr: Vr) -> Self {
        DicomElement { tag, vr, value: ElementValue::Bytes(crate::model::buffer::EmptyBuffer::shared()) }
    }

    pub fn tag(&self) -> &Tag {
        &self.tag
    }

    pub fn vr(&self) -> Vr {
        self.vr
    }

    pub fn value(&self) -> &ElementValue {
        &self.value
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// Number of values: `\`-separated count for string VRs (empty → 0),
    /// buffer size / unit size for binary VRs.
    pub fn count(&self) -> usize {
        match &self.value {
            ElementValue::Str(s) => {
                if s.is_empty() {
                    0
                } else if self.vr.is_multi_value() {
                    s.split('\\').count()
                } else {
                    // single-value text VRs (LT/ST/UR/UT): backslash is literal
                    1
                }
            }
            ElementValue::Bytes(b) => {
                if self.vr.unit_size() == 0 {
                    0
                } else {
                    (b.size() as usize) / self.vr.unit_size()
                }
            }
        }
    }

    /// The raw string form (joined multi-value), empty string for
    /// non-string VRs.
    pub fn string_value(&self) -> &str {
        self.value.as_str().unwrap_or("")
    }

    /// Splits a multi-value string into its `\`-separated parts.
    pub fn string_parts(&self) -> Vec<&str> {
        match &self.value {
            ElementValue::Str(s) if !s.is_empty() => s.split('\\').collect(),
            _ => Vec::new(),
        }
    }

    /// Serializes the value: bytes pass through; strings encode as
    /// lossy UTF-8 for now — the charset-aware encoder (ENC-01..03)
    /// replaces this at the dataset serialization layer — then pad with
    /// the VR's padding byte to even length.
    pub fn encoded(&self) -> SharedBuffer {
        match &self.value {
            ElementValue::Bytes(b) => b.clone(),
            ElementValue::Str(s) => {
                let mut bytes = s.as_bytes().to_vec();
                if bytes.len() % 2 == 1 {
                    bytes.push(self.vr.padding());
                }
                Arc::new(MemoryBuffer::from(bytes))
            }
        }
    }

    /// Typed access (ELEM-02). `index = None` reads the whole (joined)
    /// string view; `Some(i)` the i-th `\`-separated value. Fails with
    /// [`DataError::UnexpectedType`] on VR/type mismatch.
    pub fn get<T: FromElement>(&self, index: Option<usize>) -> Result<T, Error> {
        T::from_element(self, index)
    }

    /// Convenience: first value.
    pub fn first<T: FromElement>(&self) -> Result<T, Error> {
        self.get(Some(0))
    }
}

/// Conversion from an element into Rust types (ELEM-02 conversion matrix).
pub trait FromElement: Sized {
    fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error>;
}

/// Picks the requested `\`-separated part (or the joined whole when
/// `index` is `None`).
fn select(e: &DicomElement, index: Option<usize>) -> Result<String, Error> {
    let value = match &e.value {
        ElementValue::Str(s) => s.clone(),
        ElementValue::Bytes(b) => {
            // lossy text view over binary data (documented lenient read)
            String::from_utf8_lossy(&b.data()).into_owned()
        }
    };
    match index {
        None => Ok(value),
        Some(i) => {
            let parts: Vec<&str> = if value.is_empty() {
                Vec::new()
            } else {
                value.split('\\').collect()
            };
            parts.get(i).map(|s| s.to_string()).ok_or_else(|| {
                Error::Data(DataError::IndexOutOfRange {
                    tag: (&e.tag).into(),
                    index: i,
                    count: parts.len(),
                })
            })
        }
    }
}

/// Reads the `index`-th `unit_size` group of a binary value as a
/// little-endian integer/float (spec: byte order is fixed at parse time via
/// EndianBuffer wrapping; in-memory values are native LE).
fn read_le_unit<T: FromLeBytes>(e: &DicomElement, b: &SharedBuffer, index: Option<usize>) -> Result<T, Error> {
    let unit = e.vr().unit_size().max(T::SIZE);
    let i = index.unwrap_or(0);
    let start = i * unit;
    let data = b.data();
    let end = start + unit;
    if end > data.len() {
        return Err(Error::Data(DataError::IndexOutOfRange {
            tag: (&e.tag).into(),
            index: i,
            count: data.len() / unit,
        }));
    }
    Ok(T::from_le(&data[start..start + T::SIZE]))
}

/// Little-endian construction for numeric element reads.
pub trait FromLeBytes: Sized {
    const SIZE: usize;
    fn from_le(bytes: &[u8]) -> Self;
}

macro_rules! impl_le {
    ($t:ty) => {
        impl FromLeBytes for $t {
            const SIZE: usize = std::mem::size_of::<$t>();
            fn from_le(bytes: &[u8]) -> Self {
                let mut arr = [0u8; std::mem::size_of::<$t>()];
                arr.copy_from_slice(&bytes[..std::mem::size_of::<$t>()]);
                <$t>::from_le_bytes(arr)
            }
        }
    };
}
impl_le!(i16);
impl_le!(u16);
impl_le!(i32);
impl_le!(u32);
impl_le!(i64);
impl_le!(u64);
impl_le!(f32);
impl_le!(f64);

macro_rules! impl_from_int {
    ($t:ty) => {
        impl FromElement for $t {
            fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error> {
                if let ElementValue::Bytes(b) = e.value() {
                    return read_le_unit::<$t>(e, b, index);
                }
                let s = select(e, index)?;
                match e.vr() {
                    Vr::Is => parse_is(&s).and_then(|v| {
                        <$t>::try_from(v as i64).map_err(|_| type_err(e, stringify!($t)))
                    }),
                    Vr::Ds => parse_ds(&s).and_then(|f| {
                        <$t>::try_from(f as i64).map_err(|_| type_err(e, stringify!($t)))
                    }),
                    _ => s.trim().parse::<$t>().map_err(|_| type_err(e, stringify!($t))),
                }
            }
        }
    };
}

fn type_err(e: &DicomElement, expected: &'static str) -> Error {
    Error::Data(DataError::UnexpectedType {
        tag: (&e.tag).into(),
        expected,
        found: e.vr().code(),
    })
}

impl_from_int!(i16);
impl_from_int!(u16);
impl_from_int!(i32);
impl_from_int!(u32);
impl_from_int!(i64);
impl_from_int!(u64);

impl FromElement for f32 {
    fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error> {
        Ok(f64::from_element(e, index)? as f32)
    }
}

impl FromElement for f64 {
    fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error> {
        if let ElementValue::Bytes(b) = e.value() {
            return read_le_unit::<f64>(e, b, index);
        }
        let s = select(e, index)?;
        match e.vr() {
            Vr::Ds => parse_ds(&s),
            Vr::Is => parse_is(&s).map(|v| v as f64),
            _ => s.trim().parse::<f64>().map_err(|_| type_err(e, "f64")),
        }
    }
}

impl FromElement for String {
    fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error> {
        select(e, index)
    }
}

impl FromElement for Uid {
    fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error> {
        Ok((*Uid::parse(&select(e, index)?)).clone())
    }
}

impl FromElement for Arc<Uid> {
    fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error> {
        Ok(Uid::parse(&select(e, index)?))
    }
}

impl FromElement for Arc<TransferSyntax> {
    fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error> {
        crate::model::transfer_syntax::lookup(&Uid::parse(&select(e, index)?))
    }
}

impl FromElement for PersonName {
    fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error> {
        Ok(PersonName::parse(&select(e, index)?))
    }
}

impl FromElement for NaiveDate {
    fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error> {
        Ok(parse_da(&select(e, index)?)?.min)
    }
}

impl FromElement for NaiveTime {
    fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error> {
        Ok(parse_tm(&select(e, index)?)?.min)
    }
}

impl FromElement for DtPoint {
    fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error> {
        Ok(parse_dt(&select(e, index)?)?.min)
    }
}

impl<T: FromElement> FromElement for Vec<T> {
    fn from_element(e: &DicomElement, _index: Option<usize>) -> Result<Self, Error> {
        let count = if e.vr().is_string() { e.count() } else { e.count().max(1) };
        (0..count).map(|i| T::from_element(e, Some(i))).collect()
    }
}

impl<T: FromElement> FromElement for Option<T> {
    fn from_element(e: &DicomElement, index: Option<usize>) -> Result<Self, Error> {
        Ok(T::from_element(e, index).ok())
    }
}

impl FromElement for SharedBuffer {
    fn from_element(e: &DicomElement, _index: Option<usize>) -> Result<Self, Error> {
        Ok(e.encoded())
    }
}

/// Re-exports used by the dataset layer's date-time composition (DS-08).
pub mod temporal {
    pub use crate::model::dicom_datetime::{format_da, format_dt, format_tm, parse_da, parse_dt, parse_tm, DtPoint};
    pub use crate::model::range::Range;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(vr: Vr, text: &str) -> DicomElement {
        DicomElement::new_str(Tag::new(0x0010, 0x0010), vr, text)
    }

    #[test]
    fn count_and_multi_value() {
        assert_eq!(el(Vr::Lo, "A\\B\\C").count(), 3);
        assert_eq!(el(Vr::Lo, "A\\B\\C").string_parts(), vec!["A", "B", "C"]);
        assert_eq!(el(Vr::Lo, "").count(), 0, "empty element has zero values");
        assert_eq!(el(Vr::Lo, "single").count(), 1);
        // LT is single-value: the backslash is literal
        assert_eq!(el(Vr::Lt, "a\\b").count(), 1);
    }

    #[test]
    fn get_strings_and_indices() {
        let e = el(Vr::Lo, "foo\\bar");
        let s: String = e.get(None).unwrap();
        assert_eq!(s, "foo\\bar");
        let first: String = e.get(Some(0)).unwrap();
        assert_eq!(first, "foo");
        let second: String = e.get(Some(1)).unwrap();
        assert_eq!(second, "bar");
        assert!(e.get::<String>(Some(2)).is_err(), "index out of range");
    }

    #[test]
    fn get_numeric_by_vr() {
        let is = el(Vr::Is, "42\\-7");
        assert_eq!(is.first::<i32>().unwrap(), 42);
        let vals: Vec<i32> = is.get(None).unwrap();
        assert_eq!(vals, vec![42, -7]);

        let ds = el(Vr::Ds, "1,5\\2.5");
        let vals: Vec<f64> = ds.get(None).unwrap();
        assert_eq!(vals, vec![1.5, 2.5]);

        let us = DicomElement::new_bytes(
            Tag::new(0x0028, 0x0010),
            Vr::Us,
            Arc::new(MemoryBuffer::from(vec![1, 0, 2, 0])),
        );
        let vals: Vec<u16> = us.get(None).unwrap();
        assert_eq!(vals, vec![1, 2], "little-endian unit reads");
    }

    #[test]
    fn get_uid_and_transfer_syntax() {
        let ui = el(Vr::Ui, "1.2.840.10008.1.2");
        let uid: Uid = ui.first().unwrap();
        assert_eq!(uid.uid_type(), crate::model::uid::UidType::TransferSyntax);
        let ts: Arc<TransferSyntax> = ui.first().unwrap();
        assert_eq!(ts.uid(), "1.2.840.10008.1.2");
    }

    #[test]
    fn get_dates() {
        let da = el(Vr::Da, "20240305");
        let d: NaiveDate = da.first().unwrap();
        assert_eq!(d.to_string(), "2024-03-05");

        let tm = el(Vr::Tm, "103015");
        let t: NaiveTime = tm.first().unwrap();
        assert_eq!(t.to_string(), "10:30:15");

        let dt = el(Vr::Dt, "20240305103015+0100");
        let p: DtPoint = dt.first().unwrap();
        assert_eq!(p.offset.map(|o| o.local_minus_utc()), Some(3600));
    }

    #[test]
    fn get_person_name() {
        let pn = el(Vr::Pn, "Doe^John=도^존");
        let name: PersonName = pn.first().unwrap();
        assert_eq!(name.family(), Some("Doe"));
        assert_eq!(name.ideographic(), Some("도^존"));
    }

    #[test]
    fn encoded_padding() {
        let odd = el(Vr::Lo, "abc");
        assert_eq!(&*odd.encoded().data(), b"abc ");
        let even = el(Vr::Ui, "10.20.30");
        assert_eq!(&*even.encoded().data(), b"10.20.30");
        // UI pads with NUL
        let ui_odd = el(Vr::Ui, "1.2");
        assert_eq!(&*ui_odd.encoded().data(), b"1.2\x00");
    }

}


// -- content equality (DS-07) -----------------------------------------------

impl PartialEq for ElementValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ElementValue::Str(a), ElementValue::Str(b)) => a == b,
            (ElementValue::Bytes(a), ElementValue::Bytes(b)) => a.size() == b.size() && a.data() == b.data(),
            _ => false,
        }
    }
}

impl PartialEq for DicomElement {
    /// Deep content equality: tag + VR + value bytes (recursion happens at
    /// the item/dataset layers).
    fn eq(&self, other: &Self) -> bool {
        self.tag == other.tag && self.vr == other.vr && self.value == other.value
    }
}

impl Eq for DicomElement {}

// -- write-side conversion (IntoElement, DS-03 dispatch) ------------------

/// Conversion from Rust values into elements (DS-03 dispatch matrix).
/// `preferred_vr` hints the dictionary resolver; the dataset layer picks
/// the final VR from the dictionary entry whose value type matches.
pub trait IntoElement {
    fn preferred_vr(&self) -> Option<Vr>;
    fn into_element(self, tag: Tag, vr: Vr) -> DicomElement;
}

fn join_multi(parts: &[String]) -> String {
    parts.join("\\")
}

impl IntoElement for &str {
    fn preferred_vr(&self) -> Option<Vr> {
        None
    }
    fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
        DicomElement::new_str(tag, vr, self)
    }
}

impl IntoElement for String {
    fn preferred_vr(&self) -> Option<Vr> {
        None
    }
    fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
        DicomElement::new_str(tag, vr, self)
    }
}

impl IntoElement for Vec<String> {
    fn preferred_vr(&self) -> Option<Vr> {
        None
    }
    fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
        DicomElement::new_str(tag, vr, join_multi(&self))
    }
}

impl IntoElement for Uid {
    fn preferred_vr(&self) -> Option<Vr> {
        Some(Vr::Ui)
    }
    fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
        DicomElement::new_str(tag, vr, self.value().to_string())
    }
}

impl IntoElement for Arc<TransferSyntax> {
    fn preferred_vr(&self) -> Option<Vr> {
        Some(Vr::Ui)
    }
    fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
        DicomElement::new_str(tag, vr, self.uid().to_string())
    }
}

impl IntoElement for PersonName {
    fn preferred_vr(&self) -> Option<Vr> {
        Some(Vr::Pn)
    }
    fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
        DicomElement::new_str(tag, vr, self.to_string())
    }
}

macro_rules! impl_into_int_string {
    ($t:ty) => {
        impl IntoElement for $t {
            fn preferred_vr(&self) -> Option<Vr> {
                Some(Vr::Is)
            }
            fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
                match vr {
                    Vr::Is | Vr::Ds => {
                        DicomElement::new_str(tag, vr, self.to_string())
                    }
                    Vr::Ss | Vr::Us => {
                        let v = self as i64 as i16;
                        DicomElement::new_bytes(tag, vr, Arc::new(MemoryBuffer::from(v.to_le_bytes().to_vec())))
                    }
                    Vr::Sl | Vr::Ul => {
                        let v = self as i64 as i32;
                        DicomElement::new_bytes(tag, vr, Arc::new(MemoryBuffer::from(v.to_le_bytes().to_vec())))
                    }
                    Vr::Sv | Vr::Uv => {
                        let v = self as i64;
                        DicomElement::new_bytes(tag, vr, Arc::new(MemoryBuffer::from(v.to_le_bytes().to_vec())))
                    }
                    _ => DicomElement::new_str(tag, vr, self.to_string()),
                }
            }
        }
    };
}
impl_into_int_string!(i8);
impl_into_int_string!(u8);
impl_into_int_string!(i16);
impl_into_int_string!(u16);
impl_into_int_string!(i32);
impl_into_int_string!(u32);
impl_into_int_string!(i64);
impl_into_int_string!(u64);

macro_rules! impl_into_float {
    ($t:ty) => {
        impl IntoElement for $t {
            fn preferred_vr(&self) -> Option<Vr> {
                Some(Vr::Ds)
            }
            fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
                match vr {
                    Vr::Fl => {
                        let v = self as f32;
                        DicomElement::new_bytes(tag, vr, Arc::new(MemoryBuffer::from(v.to_le_bytes().to_vec())))
                    }
                    Vr::Fd => {
                        let v = self as f64;
                        DicomElement::new_bytes(tag, vr, Arc::new(MemoryBuffer::from(v.to_le_bytes().to_vec())))
                    }
                    _ => DicomElement::new_str(tag, vr, crate::model::person_name::format_ds(self as f64)),
                }
            }
        }
    };
}
impl_into_float!(f32);
impl_into_float!(f64);

impl IntoElement for NaiveDate {
    fn preferred_vr(&self) -> Option<Vr> {
        Some(Vr::Da)
    }
    fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
        let text = match vr {
            Vr::Da => crate::model::dicom_datetime::format_da(self),
            Vr::Dt => crate::model::dicom_datetime::format_dt(DtPoint {
                naive: self.and_hms_opt(0, 0, 0).expect("midnight"),
                offset: None,
            }),
            _ => crate::model::dicom_datetime::format_da(self),
        };
        DicomElement::new_str(tag, vr, text)
    }
}

impl IntoElement for NaiveTime {
    fn preferred_vr(&self) -> Option<Vr> {
        Some(Vr::Tm)
    }
    fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
        DicomElement::new_str(tag, vr, crate::model::dicom_datetime::format_tm(self))
    }
}

impl IntoElement for DtPoint {
    fn preferred_vr(&self) -> Option<Vr> {
        Some(Vr::Dt)
    }
    fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
        DicomElement::new_str(tag, vr, crate::model::dicom_datetime::format_dt(self))
    }
}

impl IntoElement for Vec<u8> {
    fn preferred_vr(&self) -> Option<Vr> {
        Some(Vr::Ob)
    }
    fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
        DicomElement::new_bytes(tag, vr, Arc::new(MemoryBuffer::from(self)))
    }
}

impl IntoElement for SharedBuffer {
    fn preferred_vr(&self) -> Option<Vr> {
        Some(Vr::Ob)
    }
    fn into_element(self, tag: Tag, vr: Vr) -> DicomElement {
        DicomElement::new_bytes(tag, vr, self)
    }
}
