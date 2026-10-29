//! DICOM data element tag (TAG-01, TAG-02; docs/01-architecture.md §3).

use crate::error::{DataError, Error, TagRef};
use crate::model::private_creator::PrivateCreator;
use std::cmp::Ordering;
use std::sync::Arc;

/// A DICOM attribute tag: `(group, element)`, optionally bound to a private
/// creator for odd (private) groups.
///
/// # Equality / ordering / hashing (TAG-02)
///
/// Mirrors fo-dicom's three-part semantics:
/// - public vs public: compare `group`, then `element`;
/// - private vs private with the same creator in the same group: compare only
///   the **low 8 bits** of `element` (block-relative, creator-defined);
/// - otherwise: `group`, then element privateness (public before private),
///   then creator string, then full element.
///
/// This keeps dataset storage stable regardless of which concrete block a
/// private tag was resolved to.
#[derive(Debug, Clone)]
pub struct Tag {
    group: u16,
    element: u16,
    private_creator: Option<Arc<PrivateCreator>>,
}

impl Tag {
    /// The unknown tag `(FFFF,FFFF)`.
    pub const UNKNOWN: Tag = Tag { group: 0xFFFF, element: 0xFFFF, private_creator: None };

    pub const fn new(group: u16, element: u16) -> Self {
        Tag { group, element, private_creator: None }
    }

    /// Constructs a private tag bound to `creator`.
    pub fn private(group: u16, element: u16, creator: Arc<PrivateCreator>) -> Self {
        debug_assert!(group & 1 == 1, "private tags require an odd group");
        Tag { group, element, private_creator: Some(creator) }
    }

    pub const fn group(&self) -> u16 {
        self.group
    }

    pub const fn element(&self) -> u16 {
        self.element
    }

    pub fn private_creator(&self) -> Option<&PrivateCreator> {
        self.private_creator.as_deref()
    }

    /// True for odd groups (PS3.5 §7.8 private data element ranges).
    pub const fn is_private(&self) -> bool {
        self.group & 1 == 1
    }

    /// True for a private creator slot: odd group, `0x0010 <= element <= 0x00FF`.
    pub const fn is_private_creator_slot(&self) -> bool {
        self.is_private() && self.element >= 0x0010 && self.element <= 0x00FF
    }

    /// Packs as `(group << 16) | element`; the creator, if any, is not part
    /// of the numeric form.
    pub const fn to_u32(&self) -> u32 {
        ((self.group as u32) << 16) | self.element as u32
    }

    /// Inverse of [`Tag::to_u32`]; the result is always a public tag.
    pub const fn from_u32(v: u32) -> Self {
        Tag::new((v >> 16) as u16, v as u16)
    }

    /// `(GGGG,EEEE)` or `(GGGG,EEEE:CREATOR)`.
    fn fmt_general(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.private_creator {
            None => write!(f, "({:04X},{:04X})", self.group, self.element),
            Some(c) => write!(f, "({:04X},{:04X}:{})", self.group, self.element, c),
        }
    }

    /// 8 hex digits `GGGGEEEE` (DICOM JSON keys).
    pub fn to_json_string(&self) -> String {
        format!("{:04X}{:04X}", self.group, self.element)
    }

    /// Formats the tag like fo-dicom `DicomTag.ToString(format)`:
    ///
    /// - `"G"` (also the fallback): `(7fe0,0010)`, private `(0029,1001:CREATOR)`
    /// - `"X"`: like `G`, but a private element shows its block offset:
    ///   `(0029,xx01:CREATOR)`
    /// - `"g"`: no parentheses, `7fe0,0010` / `0029,1001:CREATOR`
    /// - `"J"`: `7FE00010`, uppercase, no creator (the JSON key form)
    ///
    /// [`Display`](std::fmt::Display) keeps radicom's uppercase general form;
    /// this method is the fo-dicom-compatible spelling for callers that need to
    /// match its output.
    pub fn to_string_with(&self, format: &str) -> String {
        let creator = self.private_creator.as_deref().map(|c| c.as_str());
        match (format, creator) {
            ("X", Some(c)) => format!("({:04x},xx{:02x}:{})", self.group, self.element & 0xFF, c),
            ("X", None) => format!("({:04x},{:04x})", self.group, self.element),
            ("g", Some(c)) => format!("{:04x},{:04x}:{}", self.group, self.element, c),
            ("g", None) => format!("{:04x},{:04x}", self.group, self.element),
            ("J", _) => self.to_json_string(),
            (_, Some(c)) => format!("({:04x},{:04x}:{})", self.group, self.element, c),
            (_, None) => format!("({:04x},{:04x})", self.group, self.element),
        }
    }

    /// Parses `(GGGG,EEEE)`, `GGGG,EEEE`, `(GGGG,EEEE:CREATOR)` or
    /// `GGGGEEEE` (8 hex digits). Group and element are case-insensitive hex.
    pub fn parse(s: &str) -> Result<Self, Error> {
        let s = s.trim();
        // JSON form: exactly 8 hex digits, no separators.
        if !s.contains(',') {
            if s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
                let v = u32::from_str_radix(s, 16).expect("8 hex digits fit u32");
                return Ok(Tag::from_u32(v));
            }
            return Err(Error::Data(DataError::Other {
                message: format!("invalid tag \"{s}\": expected (GGGG,EEEE) or GGGGEEEE"),
            }));
        }
        let body = s.strip_prefix('(').unwrap_or(s);
        let body = body.strip_suffix(')').unwrap_or(body);
        let (gg_ee, creator) = match body.split_once(':') {
            Some((t, c)) => (t, Some(c.trim())),
            None => (body, None),
        };
        let (g, e) = gg_ee.split_once(',').ok_or_else(|| {
            Error::Data(DataError::Other {
                message: format!("invalid tag \"{s}\": missing ',' separator"),
            })
        })?;
        let group = parse_hex_u16(g).map_err(|_| bad_tag(s))?;
        let element = parse_hex_u16(e).map_err(|_| bad_tag(s))?;
        match creator.filter(|c| !c.is_empty()) {
            None => Ok(Tag::new(group, element)),
            Some(c) => Ok(Tag::private(group, element, Arc::new(PrivateCreator::new(c)))),
        }
    }
}

fn parse_hex_u16(s: &str) -> Result<u16, ()> {
    let s = s.trim();
    if s.is_empty() || s.len() > 4 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(());
    }
    u16::from_str_radix(s, 16).map_err(|_| ())
}

fn bad_tag(input: &str) -> Error {
    Error::Data(DataError::Other { message: format!("invalid tag \"{input}\"") })
}

impl PartialEq for Tag {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Tag {}

impl PartialOrd for Tag {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Tag {
    fn cmp(&self, other: &Self) -> Ordering {
        // fo-dicom `DicomTag.CompareTo`: group, then element, then (only when
        // equal) the private creator. Ordering by the full element keeps tags
        // from different private blocks distinct; the old `element & 0xFF` form
        // made `(gggg,1101:C)` and `(gggg,1201:C)` compare equal and one could
        // overwrite the other in a dataset.
        self.group
            .cmp(&other.group)
            .then_with(|| self.element.cmp(&other.element))
            .then_with(|| match (&self.private_creator, &other.private_creator) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl std::hash::Hash for Tag {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.group.hash(state);
        self.element.hash(state);
        self.private_creator.hash(state);
    }
}

impl std::fmt::Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fmt_general(f)
    }
}

impl AsRef<Tag> for Tag {
    fn as_ref(&self) -> &Tag {
        self
    }
}

impl From<&Tag> for TagRef {
    fn from(t: &Tag) -> TagRef {
        TagRef(t.group, t.element)
    }
}

impl From<u32> for Tag {
    fn from(v: u32) -> Self {
        Tag::from_u32(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn creator(name: &str) -> Arc<PrivateCreator> {
        Arc::new(PrivateCreator::new(name))
    }

    fn hash_of(t: &Tag) -> u64 {
        let mut h = DefaultHasher::new();
        t.hash(&mut h);
        h.finish()
    }

    #[test]
    fn basic_properties() {
        let t = Tag::ROWS;
        assert_eq!(t.group(), 0x0028);
        assert_eq!(t.element(), 0x0010);
        assert!(!t.is_private());
        assert!(Tag::new(0x0029, 0x0010).is_private());
        assert!(Tag::new(0x0029, 0x0010).is_private_creator_slot());
        assert!(!Tag::new(0x0029, 0x0100).is_private_creator_slot());
        assert_eq!(Tag::UNKNOWN, Tag::UNKNOWN);
    }

    #[test]
    fn u32_round_trip() {
        let t = Tag::PIXEL_DATA;
        assert_eq!(t.to_u32(), 0x7FE0_0010);
        assert_eq!(Tag::from_u32(0x7FE0_0010), t);
    }

    // §1.2 ordering table -------------------------------------------------

    #[test]
    fn ordering_public_vs_public() {
        assert!(Tag::ACCESSION_NUMBER < Tag::MODALITY);
        assert!(Tag::MODALITY < Tag::PATIENT_NAME);
        assert_eq!(Tag::PATIENT_NAME, Tag::PATIENT_NAME);
    }

    #[test]
    fn ordering_private_block_relative() {
        // Same group + same creator: the full element decides, so tags in
        // different private blocks are distinct (they must not collide).
        let c = creator("ACME");
        let a = Tag::private(0x0029, 0x1101, c.clone());
        let b = Tag::private(0x0029, 0x1201, c.clone());
        assert_ne!(a, b, "different blocks are different tags");
        assert!(a < b, "0x1101 < 0x1201 by full element");
        let c2 = Tag::private(0x0029, 0x1102, c);
        assert!(a < c2, "low byte 0x01 < 0x02");
    }

    #[test]
    fn ordering_private_different_creators() {
        let a = Tag::private(0x0029, 0x1101, creator("ACME"));
        let b = Tag::private(0x0029, 0x1101, creator("BETA"));
        assert_ne!(a, b);
        assert!(a < b, "ordered by creator string");
    }

    #[test]
    fn ordering_public_never_equals_private() {
        let a = Tag::new(0x0009, 0x0010);
        let b = Tag::private(0x0009, 0x0010, creator("ACME"));
        assert_ne!(a, b);
        assert!(a < b, "public sorts before private within a group");
    }

    #[test]
    fn eq_implies_same_hash() {
        let c1 = creator("ACME");
        let c2 = PrivateCreator::new("ACME");
        let a = Tag::private(0x0029, 0x1101, c1);
        let b = Tag::private(0x0029, 0x1101, Arc::new(c2));
        assert_eq!(a, b, "same group, element and creator content");
        assert_eq!(hash_of(&a), hash_of(&b));

        // different private blocks are different tags (full element compares)
        let other_block = Tag::private(0x0029, 0x1201, Arc::new(PrivateCreator::new("ACME")));
        assert_ne!(a, other_block);

        let p = Tag::PATIENT_NAME;
        assert_eq!(hash_of(&p), hash_of(&Tag::from_u32(p.to_u32())));
    }

    // §1.3 string formats --------------------------------------------------

    #[test]
    fn display_formats() {
        assert_eq!(Tag::ROWS.to_string(), "(0028,0010)");
        let priv_tag = Tag::private(0x0029, 0x1001, creator("Vendor X"));
        assert_eq!(priv_tag.to_string(), "(0029,1001:Vendor X)");
        assert_eq!(Tag::PIXEL_DATA.to_json_string(), "7FE00010");
    }

    #[test]
    fn parse_round_trip_all_formats() {
        assert_eq!(Tag::parse("(0028,0010)").unwrap(), Tag::ROWS);
        assert_eq!(Tag::parse("0028,0010").unwrap(), Tag::ROWS);
        assert_eq!(Tag::parse("0028,0010 ").unwrap(), Tag::ROWS);
        assert_eq!(Tag::parse("7FE00010").unwrap(), Tag::PIXEL_DATA);
        let t = Tag::parse("(0029,1001:Vendor X)").unwrap();
        assert_eq!(t, Tag::private(0x0029, 0x1001, creator("Vendor X")));
        assert_eq!(t.to_string(), "(0029,1001:Vendor X)");
    }

    #[test]
    fn the_fo_dicom_format_variants_are_available() {
        // expected strings taken from fo-dicom DicomTag.ToString(format)
        let public = Tag::PIXEL_DATA; // (7fe0,0010)
        assert_eq!(public.to_string_with("G"), "(7fe0,0010)");
        assert_eq!(public.to_string_with("X"), "(7fe0,0010)");
        assert_eq!(public.to_string_with("g"), "7fe0,0010");
        assert_eq!(public.to_string_with("J"), "7FE00010");
        assert_eq!(public.to_string_with("anything"), "(7fe0,0010)", "G is the fallback");

        let private = Tag::private(0x0029, 0x1001, creator("Vendor X"));
        assert_eq!(private.to_string_with("G"), "(0029,1001:Vendor X)");
        assert_eq!(private.to_string_with("X"), "(0029,xx01:Vendor X)", "X hides the block number");
        assert_eq!(private.to_string_with("g"), "0029,1001:Vendor X");
        assert_eq!(private.to_string_with("J"), "00291001", "J never shows the creator");
    }

    #[test]
    fn parse_errors() {
        assert!(Tag::parse("").is_err());
        assert!(Tag::parse("(0028,0010").is_ok(), "missing ')' tolerated");
        assert!(Tag::parse("(28,10)").is_ok(), "short hex tolerated");
        assert!(Tag::parse("(0028:0010)").is_err(), "missing ',' is an error");
        assert!(Tag::parse("(GGGG,EEEE)").is_err(), "non-hex is an error");
        assert!(Tag::parse("7FE0001").is_err(), "7 digits is an error");
        assert!(Tag::parse("(0028,10000)").is_err(), ">4 hex digits is an error");
    }

    #[test]
    fn sorted_collection_is_stable() {
        // Spec §1.2 ordering (fo-dicom CompareTo): group, then full element,
        // then the private creator only when the element ties.
        let mut tags = [
            Tag::private(0x0029, 0x1202, creator("ACME")),
            Tag::ROWS,
            Tag::private(0x0029, 0x1101, creator("ACME")),
            Tag::private(0x0029, 0x1101, creator("BETA")),
            Tag::ACCESSION_NUMBER,
        ];
        tags.sort();
        let rendered: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
        assert_eq!(
            rendered,
            vec![
                "(0008,0050)",
                "(0028,0010)",
                "(0029,1101:ACME)",
                "(0029,1101:BETA)",
                "(0029,1202:ACME)"
            ]
        );
    }
}
