//! Data dictionary (DICT-01, DICT-02, DICT-03; docs/01-architecture.md §2).

use crate::error::{DataError, Error};
use crate::model::private_creator::PrivateCreator;
use crate::model::tag::Tag;
use crate::model::vm::Vm;
use crate::model::vr::Vr;
use once_cell::sync::Lazy;
use radicom_dict::generated::dict::{
    RawDictEntry, DICTIONARY, MASKED_DICTIONARY, PRIVATE_DICTIONARY,
};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex, RwLock};

/// A resolved data dictionary entry.
#[derive(Debug, Clone)]
pub struct DictionaryEntry {
    /// The exact tag (for masked entries: the masked value, e.g. 0x6000).
    pub tag: Tag,
    /// Present for repeating-group entries: `(group_mask, element_mask)`.
    pub mask: Option<(u16, u16)>,
    pub creator: Option<Arc<PrivateCreator>>,
    pub name: &'static str,
    pub keyword: &'static str,
    pub vrs: Box<[Vr]>,
    pub vm: Vm,
    pub retired: bool,
}

impl DictionaryEntry {
    /// Fallback entry for tags unknown to the dictionary: accepts any VR,
    /// VM 1-n (spec §5.2 step 4).
    pub fn unknown(tag: Tag) -> Arc<DictionaryEntry> {
        static ALL_VRS: Lazy<Box<[Vr]>> = Lazy::new(|| {
            Box::from([
                Vr::Ae,
                Vr::As,
                Vr::At,
                Vr::Cs,
                Vr::Da,
                Vr::Ds,
                Vr::Dt,
                Vr::Fd,
                Vr::Fl,
                Vr::Is,
                Vr::Lo,
                Vr::Lt,
                Vr::Ob,
                Vr::Od,
                Vr::Of,
                Vr::Ol,
                Vr::Ov,
                Vr::Ow,
                Vr::Pn,
                Vr::Sh,
                Vr::Sl,
                Vr::Sq,
                Vr::Ss,
                Vr::St,
                Vr::Sv,
                Vr::Tm,
                Vr::Uc,
                Vr::Ui,
                Vr::Ul,
                Vr::Un,
                Vr::Ur,
                Vr::Us,
                Vr::Ut,
                Vr::Uv,
            ])
        });
        Arc::new(DictionaryEntry {
            tag,
            mask: None,
            creator: None,
            name: "Unknown",
            keyword: "Unknown",
            vrs: ALL_VRS.clone(),
            vm: Vm {
                min: 1,
                max: None,
                multiplicity: 1,
            },
            retired: false,
        })
    }
}

fn parse_vr_list(s: &str) -> Result<Box<[Vr]>, Error> {
    if s.is_empty() {
        // some dictionary rows (private/retired) carry no VR at all
        return Ok(Box::new([]));
    }
    s.split('\\')
        .map(|code| Vr::parse_str(code.trim()))
        .collect::<Result<Vec<_>, _>>()
        .map(Vec::into_boxed_slice)
}

fn build_entry(
    raw: &RawDictEntry,
    creator: Option<Arc<PrivateCreator>>,
) -> Result<DictionaryEntry, Error> {
    let mask = if raw.group_mask != 0xFFFF || raw.element_mask != 0xFFFF {
        Some((raw.group_mask, raw.element_mask))
    } else {
        None
    };
    Ok(DictionaryEntry {
        tag: Tag::new(raw.group, raw.element),
        mask,
        creator,
        name: raw.name,
        keyword: raw.keyword,
        vrs: parse_vr_list(raw.vr)?,
        vm: Vm::parse(raw.vm)?,
        retired: raw.retired,
    })
}

/// Masked (repeating-group) rows with an O(1) lookup index.
///
/// A masked row matches a tag when the tag's groups/elements masked by the row
/// mask equal the row's masked values. Rows fall into a small set of mask
/// shapes (`(group_mask, element_mask)`); each shape gets a hash bucket keyed by
/// the row's masked `(group, element)`. `entries` keeps insertion order so the
/// first row in the table still wins when several shapes match.
#[derive(Default)]
struct MaskedIndex {
    entries: Vec<Arc<DictionaryEntry>>,
    shapes: Vec<(u16, u16)>,
    /// `(group_mask, element_mask, masked_group, masked_element) -> first entry`.
    buckets: HashMap<(u16, u16, u16, u16), usize>,
}

impl MaskedIndex {
    fn push(&mut self, entry: Arc<DictionaryEntry>) {
        let idx = self.entries.len();
        if let Some((gm, em)) = entry.mask {
            if !self.shapes.contains(&(gm, em)) {
                self.shapes.push((gm, em));
            }
            self.buckets
                .entry((gm, em, entry.tag.group() & gm, entry.tag.element() & em))
                .or_insert(idx);
        }
        self.entries.push(entry);
    }

    /// First (lowest-index) matching entry, mirroring a linear scan.
    fn find(&self, tag: &Tag) -> Option<Arc<DictionaryEntry>> {
        let mut best: Option<usize> = None;
        for &(gm, em) in &self.shapes {
            if let Some(&idx) = self
                .buckets
                .get(&(gm, em, tag.group() & gm, tag.element() & em))
            {
                best = Some(best.map_or(idx, |b| b.min(idx)));
            }
        }
        best.map(|i| self.entries[i].clone())
    }
}

#[derive(Default)]
struct PrivateSubDict {
    exact: HashMap<u32, Arc<DictionaryEntry>>,
    masked: MaskedIndex,
}

/// The private-creator placeholder entry, applied by tag shape (odd group,
/// element `0x0010..=0x00FF`): LO VM 1. Built once — resolution runs per
/// private block, so allocating a fresh `Arc` per call was pure waste.
fn private_creator_placeholder() -> Arc<DictionaryEntry> {
    static PLACEHOLDER: Lazy<Arc<DictionaryEntry>> = Lazy::new(|| {
        Arc::new(DictionaryEntry {
            // the entry is shape-based, so the concrete tag is not meaningful here
            tag: Tag::UNKNOWN,
            mask: None,
            creator: None,
            name: "Private Creator",
            keyword: "PrivateCreator",
            vrs: Box::new([Vr::Lo]),
            vm: Vm::ONE,
            retired: false,
        })
    });
    PLACEHOLDER.clone()
}

/// The DICOM data dictionary: public entries, repeating-group (masked)
/// entries and per-creator private sub-dictionaries (DICT-01).
pub struct Dictionary {
    exact: HashMap<u32, Arc<DictionaryEntry>>,
    masked: MaskedIndex,
    keywords: HashMap<&'static str, u32>,
    private: RwLock<HashMap<String, PrivateSubDict>>,
    creators: Mutex<HashMap<String, Arc<PrivateCreator>>>,
    keyword_creator: HashMap<&'static str, &'static str>,
}

impl Dictionary {
    /// Builds a dictionary from the generated radicom-dict tables (DICT-01).
    pub fn with_generated() -> Result<Self, Error> {
        let mut dict = Dictionary {
            exact: HashMap::with_capacity(DICTIONARY.len()),
            masked: MaskedIndex::default(),
            keywords: HashMap::with_capacity(DICTIONARY.len()),
            private: RwLock::new(HashMap::with_capacity(PRIVATE_DICTIONARY.len() / 8)),
            creators: Mutex::new(HashMap::new()),
            keyword_creator: HashMap::new(),
        };
        for raw in DICTIONARY {
            let entry = Arc::new(build_entry(raw, None)?);
            dict.exact
                .insert((raw.group as u32) << 16 | raw.element as u32, entry);
            dict.keywords
                .entry(raw.keyword)
                .or_insert_with(|| (raw.group as u32) << 16 | raw.element as u32);
        }
        for raw in MASKED_DICTIONARY {
            let entry = Arc::new(build_entry(raw, None)?);
            dict.masked.push(entry);
        }
        for raw in PRIVATE_DICTIONARY {
            let creator = dict.intern_creator(raw.creator);
            let entry = Arc::new(build_entry(raw, Some(creator))?);
            let mut subs = dict.private.write().expect("private lock");
            let sub = subs.entry(raw.creator.to_string()).or_default();
            if entry.mask.is_none() {
                sub.exact
                    .insert((raw.group as u32) << 16 | raw.element as u32, entry);
            } else {
                sub.masked.push(entry);
            }
            dict.keyword_creator
                .entry(raw.keyword)
                .or_insert(raw.creator);
        }
        Ok(dict)
    }

    /// Interns a creator string so equal names share one instance (§6.4).
    pub fn intern_creator(&self, name: &str) -> Arc<PrivateCreator> {
        let mut creators = self.creators.lock().expect("creator lock");
        creators
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(PrivateCreator::new(name)))
            .clone()
    }

    /// Resolves a tag to its dictionary entry, never failing (DICT-02):
    /// private sub-dictionary (exact → masked) → private creator placeholder
    /// → public exact → public masked (first match) → Unknown fallback.
    pub fn entry(&self, tag: &Tag) -> Arc<DictionaryEntry> {
        if let Some(creator) = tag.private_creator() {
            let subs = self.private.read().expect("private lock");
            if let Some(sub) = subs.get(creator.as_str()) {
                if let Some(e) = sub.exact.get(&tag.to_u32()) {
                    return e.clone();
                }
                if let Some(e) = sub.masked.find(tag) {
                    return e;
                }
            }
            drop(subs);
        }
        if tag.is_private_creator_slot() {
            // private creator placeholder: (gggg,00xx) LO VM 1 - applies by
            // tag shape regardless of whether a creator is attached yet
            return private_creator_placeholder();
        }
        if let Some(e) = self.exact.get(&tag.to_u32()) {
            return e.clone();
        }
        if let Some(e) = self.masked.find(tag) {
            return e;
        }
        DictionaryEntry::unknown(tag.clone())
    }

    /// Reverse lookup by keyword: public dictionary first, then private
    /// rows (their creator is attached to the returned tag).
    pub fn tag_by_keyword(&self, keyword: &str) -> Option<Tag> {
        if let Some(packed) = self.keywords.get(keyword) {
            return Some(Tag::from_u32(*packed));
        }
        let creator = self.keyword_creator.get(keyword)?;
        Some(Tag::private(
            0x0001, // group is filled by the caller's private resolution; mask 0 keeps it flexible
            0x0001,
            self.intern_creator(creator),
        ))
    }

    /// Merges another dictionary's entries into this one (DICT-03):
    /// public entries replace exact matches and append masked entries;
    /// private rows join their creator sub-dictionary.
    pub fn merge_runtime(&mut self, entries: Vec<RuntimeEntry>) {
        for e in entries {
            let key = e.entry.tag.to_u32();
            if let Some(creator) = &e.entry.creator {
                let name = creator.as_str().to_string();
                let mut subs = self.private.write().expect("private lock");
                let sub = subs.entry(name).or_default();
                if e.entry.mask.is_none() {
                    sub.exact.insert(key, e.entry);
                } else {
                    sub.masked.push(e.entry);
                }
            } else if e.entry.mask.is_none() {
                self.exact.insert(key, e.entry);
            } else {
                self.masked.push(e.entry);
            }
        }
    }
}

/// A runtime-built dictionary entry (from XML loading), creator included.
pub struct RuntimeEntry {
    pub entry: Arc<DictionaryEntry>,
}

// -- default dictionary (one-shot replacement, DICT-03) -------------------

static DEFAULT: std::sync::OnceLock<Arc<Dictionary>> = std::sync::OnceLock::new();
static GENERATED: Lazy<Arc<Dictionary>> =
    Lazy::new(|| Arc::new(Dictionary::with_generated().expect("generated dictionary builds")));

/// The active default dictionary, borrowed from a process-lifetime static.
///
/// Lock-free and allocation-free: the hot path resolves one VR per element and
/// a shared `RwLock` there serializes every parsing thread.
pub fn dictionary() -> &'static Dictionary {
    match DEFAULT.get() {
        Some(d) => d,
        None => &GENERATED,
    }
}

/// The active default dictionary as an owned handle, for callers that store it.
pub fn default_dictionary() -> Arc<Dictionary> {
    match DEFAULT.get() {
        Some(d) => Arc::clone(d),
        None => Arc::clone(&GENERATED),
    }
}

/// Replaces the default dictionary; only the first call wins (subsequent
/// calls error, mirroring fo-dicom's one-shot guard).
pub fn set_default_dictionary(dict: Arc<Dictionary>) -> Result<(), Error> {
    DEFAULT.set(dict).map_err(|_| {
        Error::Data(DataError::Other {
            message: "default dictionary already replaced".into(),
        })
    })
}

// -- runtime XML loading (DICT-03) ----------------------------------------

/// Loads dictionary entries from the fo-dicom XML shape. Gzip input
/// (`.gz` files, magic `1F 8B`) is auto-detected and decompressed. Public
/// and private (`<dictionary creator="...">`) entries are returned
/// together; merge them with [`Dictionary::merge_runtime`].
///
/// Note: keyword/name strings are leaked into `'static` — dictionary
/// loading is a rare, one-shot operation, keeping [`DictionaryEntry`]
/// `'static`-friendly like the generated tables.
pub fn load_xml(reader: impl Read) -> Result<Vec<RuntimeEntry>, Error> {
    let mut bytes = Vec::new();
    let mut reader = reader;
    reader.read_to_end(&mut bytes).map_err(Error::from)?;
    if bytes.starts_with(&[0x1F, 0x8B]) {
        let mut decoded = Vec::new();
        flate2::read::MultiGzDecoder::new(bytes.as_slice())
            .read_to_end(&mut decoded)
            .map_err(Error::from)?;
        load_xml_stream(decoded.as_slice())
    } else {
        load_xml_stream(bytes.as_slice())
    }
}

struct PendingTag {
    group: u16,
    element: u16,
    group_mask: u16,
    element_mask: u16,
    keyword: &'static str,
    vr: &'static str,
    vm: &'static str,
    retired: bool,
    name: &'static str,
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn load_xml_stream(reader: impl Read) -> Result<Vec<RuntimeEntry>, Error> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(std::io::BufReader::new(reader));
    reader.config_mut().trim_text(true);
    let mut out: Vec<RuntimeEntry> = Vec::new();
    let mut creator: Option<Arc<PrivateCreator>> = None;
    let mut interner: HashMap<String, Arc<PrivateCreator>> = HashMap::new();
    let mut pending: Option<PendingTag> = None;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"dictionary" => {
                    creator = None;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"creator" {
                            let name = attr
                                .decode_and_unescape_value(reader.decoder())
                                .map_err(Error::from)?
                                .into_owned();
                            creator = Some(
                                interner
                                    .entry(name.clone())
                                    .or_insert_with(|| Arc::new(PrivateCreator::new(name)))
                                    .clone(),
                            );
                        }
                    }
                }
                b"tag" => {
                    let mut group = None;
                    let mut element = None;
                    let mut keyword = String::new();
                    let mut vr = String::new();
                    let mut vm = String::new();
                    let mut retired = false;
                    for attr in e.attributes().flatten() {
                        let val = attr
                            .decode_and_unescape_value(reader.decoder())
                            .map_err(Error::from)?
                            .into_owned();
                        match attr.key.as_ref() {
                            b"group" => group = Some(parse_hex_mask(&val)),
                            b"element" => element = Some(parse_hex_mask(&val)),
                            b"keyword" => keyword = val,
                            b"vr" => {
                                vr = val
                                    .split('/')
                                    .map(|s| s.trim())
                                    .collect::<Vec<_>>()
                                    .join("\\")
                            }
                            b"vm" => vm = val,
                            b"retired" => retired = val == "true",
                            _ => {}
                        }
                    }
                    let ((g, gm), (el, em)) = match (group, element) {
                        (Some(g), Some(el)) => (g, el),
                        _ => {
                            return Err(Error::Data(DataError::Other {
                                message: "dictionary XML tag missing group/element".into(),
                            }))
                        }
                    };
                    pending = Some(PendingTag {
                        group: g,
                        element: el,
                        group_mask: gm,
                        element_mask: em,
                        keyword: leak(keyword),
                        vr: leak(vr),
                        vm: leak(vm),
                        retired,
                        name: "",
                    });
                }
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if let Some(p) = pending.as_mut() {
                    p.name = leak(t.unescape().map_err(Error::from)?.into_owned());
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"tag" {
                    if let Some(p) = pending.take() {
                        let mask = if p.group_mask != 0xFFFF || p.element_mask != 0xFFFF {
                            Some((p.group_mask, p.element_mask))
                        } else {
                            None
                        };
                        out.push(RuntimeEntry {
                            entry: Arc::new(DictionaryEntry {
                                tag: Tag::new(p.group, p.element),
                                mask,
                                creator: creator.clone(),
                                name: p.name,
                                keyword: p.keyword,
                                vrs: parse_vr_list(p.vr)?,
                                vm: Vm::parse(p.vm)?,
                                retired: p.retired,
                            }),
                        });
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(Error::from(e)),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Parses up to 4 hex digits with `x` wildcards → `(value, mask)`.
fn parse_hex_mask(hex: &str) -> (u16, u16) {
    let mut value = 0u16;
    let mut mask = 0u16;
    for c in hex.chars() {
        value <<= 4;
        mask <<= 4;
        let d = if c == 'x' || c == 'X' {
            0
        } else {
            c.to_digit(16).unwrap_or(0) as u16
        };
        value |= d;
        mask |= if c == 'x' || c == 'X' { 0x0 } else { 0xF };
    }
    (value, mask)
}

impl From<quick_xml::Error> for Error {
    fn from(e: quick_xml::Error) -> Self {
        Error::Data(DataError::Other {
            message: format!("dictionary XML: {e}"),
        })
    }
}

impl std::fmt::Display for DictionaryEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.tag, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_dictionary_lookup_precedence() {
        let dict = Dictionary::with_generated().unwrap();

        // exact public hit
        let pixel = dict.entry(&Tag::PIXEL_DATA);
        assert_eq!(pixel.keyword, "PixelData");
        assert_eq!(&*pixel.vrs, &[Vr::Ob, Vr::Ow]);
        assert_eq!(pixel.vm, Vm::ONE);

        // masked 60xx hit
        let overlay = dict.entry(&Tag::new(0x6014, 0x3000));
        assert_eq!(overlay.keyword, "OverlayData");
        assert_eq!(overlay.mask, Some((0xFF00, 0xFFFF)));

        // private creator slot → placeholder entry
        let slot = dict.entry(&Tag::new(0x0029, 0x0010));
        assert_eq!(slot.keyword, "PrivateCreator");
        assert_eq!(&*slot.vrs, &[Vr::Lo]);

        // private tag without loaded creator → placeholder (creator slot
        // resolution happens at the dataset layer; raw slot 0x0010-0x00FF)
        // private exact miss + masked private miss → public masked → unknown
        let unknown = dict.entry(&Tag::new(0x0043, 0x9999));
        assert_eq!(unknown.keyword, "Unknown");
        assert_eq!(unknown.vrs.len(), 34, "fallback accepts every VR");
    }

    #[test]
    fn private_sub_dictionary_lookup() {
        let dict = Dictionary::with_generated().unwrap();
        // pick a real private row from the generated table
        let (creator, raw) = {
            let row = &radicom_dict::generated::dict::PRIVATE_DICTIONARY[0];
            (row.creator, row)
        };
        let creator_arc = dict.intern_creator(creator);
        let tag = Tag::private(raw.group, raw.element, creator_arc.clone());
        let entry = dict.entry(&tag);
        assert_eq!(entry.keyword, raw.keyword, "private exact hit");
        assert_eq!(entry.name, raw.name);
        assert_eq!(entry.creator.as_ref().map(|c| c.as_str()), Some(creator));
    }

    #[test]
    fn keyword_reverse_lookup() {
        let dict = Dictionary::with_generated().unwrap();
        assert_eq!(dict.tag_by_keyword("PatientID"), Some(Tag::PATIENT_ID));
        assert_eq!(dict.tag_by_keyword("PixelData"), Some(Tag::PIXEL_DATA));
        assert_eq!(dict.tag_by_keyword("NoSuchKeyword"), None);
    }

    #[test]
    fn creator_interning() {
        let dict = Dictionary::with_generated().unwrap();
        let a = dict.intern_creator("ACME 1.2");
        let b = dict.intern_creator("ACME 1.2");
        assert!(Arc::ptr_eq(&a, &b), "same name interns to one instance");
    }

    #[test]
    fn default_dictionary_one_shot_replacement() {
        // active default resolves generated entries
        assert_eq!(
            default_dictionary().entry(&Tag::PIXEL_DATA).keyword,
            "PixelData"
        );

        let custom = Dictionary::with_generated().unwrap();
        set_default_dictionary(Arc::new(custom)).unwrap();
        assert_eq!(
            default_dictionary().entry(&Tag::PIXEL_DATA).keyword,
            "PixelData"
        );

        // second replacement errors (one-shot guard)
        let again = Dictionary::with_generated().unwrap();
        assert!(set_default_dictionary(Arc::new(again)).is_err());
    }

    #[test]
    fn load_runtime_xml_and_merge() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<dictionary>
  <tag group="0008" element="0005" keyword="SpecificCharacterSet" vr="CS" vm="1">Specific Character Set</tag>
</dictionary>"#;
        let entries = load_xml(xml.as_bytes()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry.keyword, "SpecificCharacterSet");
        assert_eq!(&*entries[0].entry.vrs, &[Vr::Cs]);

        let private_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<dictionaries>
  <dictionary creator="ACME 1.2">
    <tag group="0029" element="1001" keyword="AcmeWidget" vr="LO" vm="1">Acme Widget</tag>
  </dictionary>
</dictionaries>"#;
        let rows = load_xml(private_xml.as_bytes()).unwrap();
        assert_eq!(rows.len(), 1);
        let creator = rows[0]
            .entry
            .creator
            .as_ref()
            .expect("private creator attached");
        assert_eq!(creator.as_str(), "ACME 1.2");

        // merge and resolve through a private tag
        let mut dict = Dictionary::with_generated().unwrap();
        dict.merge_runtime(rows);
        let tag = Tag::private(0x0029, 0x1001, dict.intern_creator("ACME 1.2"));
        assert_eq!(dict.entry(&tag).keyword, "AcmeWidget");
    }

    #[test]
    fn gzip_xml_auto_detected() {
        let xml = r#"<dictionary><tag group="0010" element="0020" keyword="PatientID" vr="LO" vm="1">Patient ID</tag></dictionary>"#;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, xml.as_bytes()).unwrap();
        let gz = encoder.finish().unwrap();
        let entries = load_xml(gz.as_slice()).unwrap();
        assert_eq!(entries[0].entry.keyword, "PatientID");
    }

    #[test]
    fn private_creator_placeholder_is_built_once() {
        let dict = Dictionary::with_generated().unwrap();
        let a = dict.entry(&Tag::new(0x0029, 0x0010));
        let b = dict.entry(&Tag::new(0x0031, 0x0010));
        assert!(
            Arc::ptr_eq(&a, &b),
            "placeholder is a shared static, not a per-call Arc"
        );
        assert_eq!(a.keyword, "PrivateCreator");
        assert_eq!(&*a.vrs, &[Vr::Lo]);
        assert_eq!(a.vm, Vm::ONE);
    }

    #[test]
    fn masked_index_prefers_first_row_across_shapes() {
        let mk = |element: u16, gm: u16, em: u16, kw: &'static str| {
            Arc::new(DictionaryEntry {
                tag: Tag::new(0x0029, element),
                mask: Some((gm, em)),
                creator: None,
                name: kw,
                keyword: kw,
                vrs: Box::new([Vr::Lo]),
                vm: Vm::ONE,
                retired: false,
            })
        };
        let mut idx = MaskedIndex::default();
        idx.push(mk(0x10F0, 0xFFFF, 0x00F0, "GeneralFirst")); // matches 0x10FF
        idx.push(mk(0x10FF, 0xFFFF, 0x00FF, "BlockLater")); // also matches 0x10FF
        assert_eq!(
            idx.find(&Tag::new(0x0029, 0x10FF)).unwrap().keyword,
            "GeneralFirst",
            "the earlier row wins even when the later one has a faster mask shape"
        );

        let mut idx2 = MaskedIndex::default();
        idx2.push(mk(0x1005, 0xFFFF, 0x00FF, "Block"));
        assert_eq!(
            idx2.find(&Tag::new(0x0029, 0x1205)).unwrap().keyword,
            "Block"
        );
    }

    #[test]
    fn resolution_order_is_private_exact_masked_placeholder_public_exact_masked_unknown() {
        let creator = Arc::new(PrivateCreator::new("ACME"));
        let pub_exact_tag = Tag::new(0x0010, 0x0020);
        let pub_masked_tag = Tag::new(0x6000, 0x0010);
        let mk = |tag: Tag, mask: Option<(u16, u16)>, kw: &'static str| {
            Arc::new(DictionaryEntry {
                tag,
                mask,
                creator: None,
                name: kw,
                keyword: kw,
                vrs: Box::new([Vr::Lo]),
                vm: Vm::ONE,
                retired: false,
            })
        };
        let mut exact = HashMap::new();
        exact.insert(
            pub_exact_tag.to_u32(),
            mk(pub_exact_tag.clone(), None, "PublicExact"),
        );
        let mut masked = MaskedIndex::default();
        masked.push(mk(
            pub_masked_tag.clone(),
            Some((0xFF00, 0xFFFF)),
            "PublicMasked",
        ));
        let mut sub = PrivateSubDict::default();
        sub.exact.insert(
            0x0029_1001,
            mk(Tag::new(0x0029, 0x1001), None, "PrivateExact"),
        );
        sub.masked.push(mk(
            Tag::new(0x0029, 0x1000),
            Some((0xFFFF, 0x00FF)),
            "PrivateMasked",
        ));
        let mut private = HashMap::new();
        private.insert("ACME".to_string(), sub);
        let dict = Dictionary {
            exact,
            masked,
            keywords: HashMap::new(),
            private: RwLock::new(private),
            creators: Mutex::new(HashMap::new()),
            keyword_creator: HashMap::new(),
        };

        let p = |element: u16| Tag::private(0x0029, element, creator.clone());
        assert_eq!(dict.entry(&p(0x1001)).keyword, "PrivateExact");
        assert_eq!(dict.entry(&p(0x1000)).keyword, "PrivateMasked");
        assert_eq!(
            dict.entry(&p(0x0010)).keyword,
            "PrivateCreator",
            "creator slot placeholder"
        );
        assert_eq!(dict.entry(&pub_exact_tag).keyword, "PublicExact");
        assert_eq!(
            dict.entry(&Tag::new(0x6014, 0x0010)).keyword,
            "PublicMasked"
        );
        assert_eq!(dict.entry(&Tag::new(0x0043, 0x9999)).keyword, "Unknown");
    }
}
