//! The dataset container (DS-01..05, DS-09; docs/01-architecture.md §3).

use crate::error::{DataError, Error, TagRef};
use crate::model::buffer::SharedBuffer;
use crate::model::element::{DicomElement, FromElement, IntoElement};
use crate::model::item::{DicomFragmentSequence, DicomItem, DicomSequence};
use crate::model::masked_tag::MaskedTag;
use crate::model::tag::Tag;
use crate::model::transfer_syntax::{syntaxes, TransferSyntax};
use crate::model::vr::Vr;
use std::collections::BTreeMap;
use std::sync::Arc;

/// An ordered DICOM dataset: items sorted by tag (iteration equals file
/// order), plus its transfer syntax and validation switch (DS-01).
#[derive(Debug, Clone)]
pub struct Dataset {
    items: BTreeMap<Tag, DicomItem>,
    transfer_syntax: Arc<TransferSyntax>,
    validate: bool,
}

impl Default for Dataset {
    fn default() -> Self {
        Dataset::new()
    }
}

impl Dataset {
    /// Empty dataset with Explicit VR Little Endian.
    pub fn new() -> Self {
        Dataset {
            items: BTreeMap::new(),
            transfer_syntax: syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            validate: true,
        }
    }

    pub fn with_transfer_syntax(ts: Arc<TransferSyntax>) -> Self {
        Dataset { items: BTreeMap::new(), transfer_syntax: ts, validate: true }
    }

    pub fn transfer_syntax(&self) -> Arc<TransferSyntax> {
        self.transfer_syntax.clone()
    }

    /// Setting the syntax cascades into nested sequences (DS-01).
    pub fn set_transfer_syntax(&mut self, ts: Arc<TransferSyntax>) {
        self.transfer_syntax = ts.clone();
        for item in self.items.values_mut() {
            if let DicomItem::Sequence(seq) = item {
                for ds in seq.items_mut() {
                    ds.set_transfer_syntax(ts.clone());
                }
            }
        }
    }

    pub fn validation_enabled(&self) -> bool {
        self.validate
    }

    pub fn set_validation(&mut self, on: bool) {
        self.validate = on;
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Ascending tag order iteration (invariant: file write order).
    pub fn items(&self) -> impl Iterator<Item = &DicomItem> {
        self.items.values()
    }

    /// Items whose tag is in `group`, excluding the group-length element
    /// `(gggg,0000)` (DS-10). In ascending tag order.
    pub fn enumerate_group(&self, group: u16) -> impl Iterator<Item = &DicomItem> {
        self.items.values().filter(move |item| {
            let tag = item.tag();
            tag.group() == group && tag.element() != 0x0000
        })
    }

    /// Items whose tag matches `masked` (e.g. `(60xx,3000)`) (DS-10).
    /// In ascending tag order.
    pub fn enumerate_masked<'a>(
        &'a self,
        masked: &'a MaskedTag,
    ) -> impl Iterator<Item = &'a DicomItem> + 'a {
        self.items
            .values()
            .filter(move |item| masked.matches(item.tag().group(), item.tag().element()))
    }

    pub fn contains(&self, tag: &Tag) -> bool {
        let tag = self.normalize_read(tag);
        self.items.contains_key(&tag)
    }

    pub fn item(&self, tag: &Tag) -> Option<&DicomItem> {
        let tag = self.normalize_read(tag);
        self.items.get(&tag)
    }

    // -- reads (DS-02, three tiers) ----------------------------------------

    /// Reads a typed value; missing tag or conversion failure → `Err`.
    pub fn get<T: FromElement>(&self, tag: impl AsRef<Tag>) -> Result<T, Error> {
        let tag = self.normalize_read(tag.as_ref());
        let e = self.element_of(&tag)?;
        e.get(None)
    }

    /// Reads a typed value at a specific `\`-separated index.
    pub fn get_at<T: FromElement>(&self, tag: impl AsRef<Tag>, index: usize) -> Result<T, Error> {
        let tag = self.normalize_read(tag.as_ref());
        let e = self.element_of(&tag)?;
        e.get(Some(index))
    }

    /// `Option` tier: any failure → `None`.
    pub fn try_get<T: FromElement>(&self, tag: impl AsRef<Tag>) -> Option<T> {
        self.get(tag).ok()
    }

    /// Default tier: any failure → `default`.
    ///
    /// A missing tag is the ordinary case and stays silent. An attribute that
    /// is present but will not convert is a data-model problem, so it is logged
    /// at debug level instead of vanishing without trace.
    pub fn get_or<T: FromElement>(&self, tag: impl AsRef<Tag>, default: T) -> T {
        let tag = tag.as_ref();
        match self.get(tag) {
            Ok(value) => value,
            Err(error) => {
                if self.contains(tag) {
                    tracing::debug!(tag = %tag, %error, "attribute does not convert, using the default");
                }
                default
            }
        }
    }

    /// Unified string view (joined multi-value); missing tag → `Err`.
    pub fn get_str(&self, tag: impl AsRef<Tag>) -> Result<String, Error> {
        self.get::<String>(tag)
    }

    pub fn try_get_str(&self, tag: impl AsRef<Tag>) -> Option<String> {
        self.try_get(tag)
    }

    /// Raw element access (zero-copy buffer reads via `get::<SharedBuffer>`).
    pub fn get_element(&self, tag: impl AsRef<Tag>) -> Result<&DicomElement, Error> {
        let tag = self.normalize_read(tag.as_ref());
        self.element_of(&tag)
    }

    /// Value count: element value count or sequence item count.
    pub fn value_count(&self, tag: impl AsRef<Tag>) -> Result<usize, Error> {
        let tag = self.normalize_read(tag.as_ref());
        match self.items.get(&tag) {
            Some(DicomItem::Element(e)) => Ok(e.count()),
            Some(DicomItem::Sequence(s)) => Ok(s.len()),
            Some(DicomItem::Fragments(f)) => Ok(f.fragments().len()),
            None => Err(Error::Data(DataError::TagNotFound { tag: TagRef(tag.group(), tag.element()) })),
        }
    }

    /// The SQ item list; missing or wrong type → `Err`.
    pub fn get_sequence(&self, tag: impl AsRef<Tag>) -> Result<&DicomSequence, Error> {
        let tag = self.normalize_read(tag.as_ref());
        match self.items.get(&tag) {
            Some(DicomItem::Sequence(s)) => Ok(s),
            Some(_) => Err(Error::Data(DataError::UnexpectedType {
                tag: TagRef(tag.group(), tag.element()),
                expected: "SQ",
                found: "element",
            })),
            None => Err(Error::Data(DataError::TagNotFound { tag: TagRef(tag.group(), tag.element()) })),
        }
    }

    pub fn try_get_sequence(&self, tag: impl AsRef<Tag>) -> Option<&DicomSequence> {
        self.get_sequence(tag).ok()
    }

    fn element_of(&self, tag: &Tag) -> Result<&DicomElement, Error> {
        match self.items.get(tag) {
            Some(DicomItem::Element(e)) => Ok(e),
            Some(DicomItem::Sequence(_)) => Err(Error::Data(DataError::UnexpectedType {
                tag: TagRef(tag.group(), tag.element()),
                expected: "element",
                found: "SQ",
            })),
            Some(DicomItem::Fragments(_)) => Err(Error::Data(DataError::UnexpectedType {
                tag: TagRef(tag.group(), tag.element()),
                expected: "element",
                found: "fragments",
            })),
            None => Err(Error::Data(DataError::TagNotFound {
                tag: TagRef(tag.group(), tag.element()),
            })),
        }
    }

    // -- writes (DS-03) ------------------------------------------------------

    /// Inserts an item; duplicate tags error (DS-03, fo-dicom Add semantics).
    pub fn insert(&mut self, item: impl Into<DicomItem>) -> Result<&mut Self, Error> {
        let item: DicomItem = item.into();
        let tag = self.resolve_private_write(item.tag().clone(), false);
        match tag {
            Some(t) => {
                if self.items.contains_key(&t) {
                    return Err(Error::Data(DataError::TagAlreadyExists {
                        tag: TagRef(t.group(), t.element()),
                    }));
                }
                self.items.insert(t, item);
                Ok(self)
            }
            None => Err(Error::Data(DataError::PrivateTagResolution {
                tag: TagRef(item.tag().group(), item.tag().element()),
            })),
        }
    }

    /// Inserts or replaces (fo-dicom AddOrUpdate semantics).
    ///
    /// A dictionary-style private tag allocates its creator block on demand.
    pub fn insert_or_update(&mut self, item: impl Into<DicomItem>) -> &mut Self {
        let item: DicomItem = item.into();
        if let Some(t) = self.resolve_private_write(item.tag().clone(), true) {
            self.items.insert(t.clone(), item.with_tag(t));
        }
        self
    }

    /// Typed insert with dictionary VR resolution (DS-03 dispatch).
    pub fn insert_value<V: IntoElement>(&mut self, tag: Tag, value: V) -> &mut Self {
        let vr = self.resolve_vr(&tag, value.preferred_vr());
        self.insert_or_update(value.into_element(tag, vr))
    }

    /// Typed insert erroring on duplicate tags (Add semantics).
    pub fn insert_new<V: IntoElement>(&mut self, tag: Tag, value: V) -> Result<&mut Self, Error> {
        let vr = self.resolve_vr(&tag, value.preferred_vr());
        self.insert(value.into_element(tag, vr))
    }

    /// Inserts an empty (zero-length) element — the "null" semantics of
    /// fo-dicom (DS-03).
    pub fn insert_empty(&mut self, tag: Tag, vr: Vr) -> &mut Self {
        self.insert_or_update(DicomElement::new_empty(tag, vr))
    }

    /// Convenience: string insert with an explicit VR.
    pub fn insert_str(&mut self, tag: Tag, vr: Vr, text: impl Into<String>) -> &mut Self {
        self.insert_or_update(DicomElement::new_str(tag, vr, text))
    }

    /// Appends a sequence and returns it for item population.
    pub fn insert_sequence(&mut self, tag: Tag) -> &mut DicomSequence {
        let seq = DicomSequence::new(tag.clone());
        self.insert_or_update(DicomItem::Sequence(seq));
        match self.items.get_mut(&tag) {
            Some(DicomItem::Sequence(s)) => s,
            _ => unreachable!("just inserted"),
        }
    }

    /// Appends a fragment sequence for encapsulated pixel data.
    pub fn insert_fragments(&mut self, tag: Tag, vr: Vr) -> &mut DicomFragmentSequence {
        let frags = DicomFragmentSequence::new(tag.clone(), vr);
        self.insert_or_update(DicomItem::Fragments(frags));
        match self.items.get_mut(&tag) {
            Some(DicomItem::Fragments(f)) => f,
            _ => unreachable!("just inserted"),
        }
    }

    pub fn remove(&mut self, tag: impl AsRef<Tag>) -> Option<DicomItem> {
        let tag = self.normalize_read(tag.as_ref());
        self.items.remove(&tag)
    }

    pub fn retain(&mut self, mut pred: impl FnMut(&DicomItem) -> bool) {
        self.items.retain(|_, item| pred(item));
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Copies all (or the selected) items into `dest` (DS-05).
    pub fn copy_to(&self, dest: &mut Dataset) {
        for (tag, item) in &self.items {
            dest.items.insert(tag.clone(), item.clone());
        }
    }

    // -- private tags (DS-04) --------------------------------------------------

    /// Scans the group's creator slots for `creator` and returns the block
    /// number (0x10..=0xFF).
    fn find_private_block(&self, creator: &str, group: u16) -> Option<u16> {
        for block in 0x10u16..=0xFF {
            // the creator slot element number IS the block number
            let slot = Tag::new(group, block);
            if let Some(DicomItem::Element(e)) = self.items.get(&slot) {
                if e.string_value().trim() == creator {
                    return Some(block);
                }
            }
        }
        None
    }

    /// Resolves a dictionary-style private tag (element ≤ 0x00FF) to its
    /// concrete block tag by scanning the group's creator slots. With
    /// `create`, a missing creator allocates the first free block and
    /// writes the PrivateCreator element; a full group returns `None`.
    pub fn get_private_tag(&mut self, tag: Tag, create: bool) -> Option<Tag> {
        if !tag.is_private() || tag.element() > 0x00FF {
            return Some(tag);
        }
        let creator_arc = match tag.private_creator() {
            Some(c) => Arc::new(c.clone()),
            None => return Some(tag),
        };
        let creator = creator_arc.as_str();
        let group = tag.group();
        let low = tag.element() & 0xFF;
        if let Some(block) = self.find_private_block(creator, group) {
            return Some(Tag::private(group, block << 8 | low, creator_arc));
        }
        if create {
            for block in 0x10u16..=0xFF {
                // the creator slot element number IS the block number; claim
                // the first slot that has no entry yet
                let slot = Tag::new(group, block);
                let entry = self.items.entry(slot).or_insert_with(|| {
                    DicomItem::Element(DicomElement::new_str(
                        Tag::new(group, block),
                        Vr::Lo,
                        creator.to_string(),
                    ))
                });
                if let DicomItem::Element(e) = entry {
                    if e.string_value().trim() == creator {
                        return Some(Tag::private(group, block << 8 | low, creator_arc));
                    }
                }
            }
        }
        None
    }

    /// Read-side private normalization (DS-04): dictionary-style private
    /// tags resolve to their concrete block tag when the creator block is
    /// present; otherwise the tag passes through unchanged.
    fn normalize_read(&self, tag: &Tag) -> Tag {
        if tag.is_private() && tag.element() <= 0x00FF {
            if let Some(creator) = tag.private_creator() {
                if let Some(block) = self.find_private_block(creator.as_str(), tag.group()) {
                    return Tag::private(
                        tag.group(),
                        block << 8 | (tag.element() & 0xFF),
                        Arc::new(creator.clone()),
                    );
                }
            }
        }
        tag.clone()
    }

    // -- validation (VAL-01..03) ------------------------------------------------

    /// Validates every element's content and value multiplicity, recursing into
    /// sequences (VAL-01/02); returns the first [`crate::error::ValidationFailure`].
    ///
    /// Validation is on demand, mirroring fo-dicom's `Validate()`: the write path
    /// and the file parser do **not** call it, so tolerant round-trips of
    /// real-world files keep working. Runs only when the per-dataset flag and the
    /// global switch are both on (`should_validate`); returns `Ok(())` otherwise.
    pub fn validate(&self) -> Result<(), Error> {
        if !crate::validation::should_validate(self.validate) {
            return Ok(());
        }
        self.validate_items()
    }

    fn validate_items(&self) -> Result<(), Error> {
        for item in self.items.values() {
            match item {
                DicomItem::Element(e) => crate::validation::validate_element(e)?,
                DicomItem::Sequence(s) => {
                    for item_ds in s.items() {
                        item_ds.validate_items()?;
                    }
                }
                DicomItem::Fragments(_) => {}
            }
        }
        Ok(())
    }

    // -- walker (DS-09) ----------------------------------------------------------

    /// Depth-first traversal in tag order; shared by the writer, dump and
    /// JSON serializer.
    pub fn walk(&self, walker: &mut impl DatasetWalker) {
        for item in self.items.values() {
            match item {
                DicomItem::Element(e) => walker.on_element(e),
                DicomItem::Sequence(s) => {
                    walker.on_sequence_begin(s);
                    for item_ds in s.items() {
                        walker.on_sequence_item_begin(item_ds);
                        item_ds.walk(walker);
                        walker.on_sequence_item_end(item_ds);
                    }
                    walker.on_sequence_end(s);
                }
                DicomItem::Fragments(f) => {
                    walker.on_fragments_begin(f);
                    for fragment in f.fragments() {
                        walker.on_fragment(f, fragment);
                    }
                    walker.on_fragments_end(f);
                }
            }
        }
    }

    // -- internals ---------------------------------------------------------------

    fn resolve_vr(&self, tag: &Tag, preferred: Option<Vr>) -> Vr {
        // The dictionary is authoritative for known tags. The value's own
        // preference is only a hint: a `u16` written to ROWS must produce US
        // (the standard VR), not IS. Letting the preference always win wrote
        // non-conformant bytes that still round-tripped because reader and
        // writer shared the same mistake.
        let entry = crate::model::dictionary::dictionary().entry(tag);
        let Some(dict_vr) = entry.vrs.first().copied() else {
            return preferred.unwrap_or(Vr::Un);
        };
        match preferred {
            Some(p) if p == dict_vr || numeric_hint_fits(p, dict_vr) => dict_vr,
            Some(p) => p,
            None => dict_vr,
        }
    }

    /// Write-side normalization: dictionary-style private tags (element
    /// ≤ 0x00FF with a creator) resolve to their concrete block tags; when
    /// resolution fails with `create = true` the creator block is claimed.
    fn resolve_private_write(&mut self, tag: Tag, create: bool) -> Option<Tag> {
        self.get_private_tag(tag, create)
    }
}

/// Whether a numeric value hint (`IS`/`DS`) may be replaced by the dictionary's
/// numeric VR.
///
/// Only numeric hints are interchangeable with numeric VRs. A string hint on a
/// binary tag (`OB`, `OW`, `AT`, ...) must stay the caller's choice, because the
/// value cannot be silently re-encoded.
fn numeric_hint_fits(hint: Vr, dict_vr: Vr) -> bool {
    use Vr::*;
    match hint {
        Is => matches!(dict_vr, Is | Us | Ss | Ul | Sl | Uv | Sv),
        Ds => matches!(dict_vr, Ds | Fl | Fd),
        _ => false,
    }
}

/// Deep-traversal callback trait (DS-09).
pub trait DatasetWalker {
    fn on_element(&mut self, element: &DicomElement);
    fn on_sequence_begin(&mut self, _sequence: &DicomSequence) {}
    fn on_sequence_item_begin(&mut self, _item: &Dataset) {}
    fn on_sequence_item_end(&mut self, _item: &Dataset) {}
    fn on_sequence_end(&mut self, _sequence: &DicomSequence) {}
    fn on_fragments_begin(&mut self, _fragments: &DicomFragmentSequence) {}
    fn on_fragment(&mut self, _fragments: &DicomFragmentSequence, _buffer: &SharedBuffer) {}
    fn on_fragments_end(&mut self, _fragments: &DicomFragmentSequence) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::buffer::MemoryBuffer;

    const PATIENT_NAME: Tag = Tag::PATIENT_NAME;
    const PATIENT_ID: Tag = Tag::PATIENT_ID;
    const ROWS: Tag = Tag::ROWS;
    const COLUMNS: Tag = Tag::COLUMNS;

    #[test]
    fn three_tier_reads() {
        let mut ds = Dataset::new();
        ds.insert_str(PATIENT_ID, Vr::Lo, "PID-1");
        ds.insert_str(ROWS, Vr::Is, "512");

        // tier 1: Err on missing
        assert!(ds.get::<String>(Tag::MODALITY).is_err());
        // tier 2: None
        assert_eq!(ds.try_get::<String>(Tag::MODALITY), None);
        // tier 3: default
        assert_eq!(ds.get_or(Tag::MODALITY, "none".to_string()), "none");

        assert_eq!(ds.get_str(PATIENT_ID).unwrap(), "PID-1");
        let rows: u16 = ds.get(ROWS).unwrap();
        assert_eq!(rows, 512, "IS parses to numeric");
    }

    #[test]
    fn group_and_masked_enumeration() {
        let mut ds = Dataset::new();
        // patient group, plus its group-length element which must be excluded
        ds.insert_empty(Tag::new(0x0010, 0x0000), Vr::Ul); // group length
        ds.insert_str(PATIENT_NAME, Vr::Pn, "DOE^JOHN");
        ds.insert_str(PATIENT_ID, Vr::Lo, "PID-1");
        // two overlay groups (60xx) with the same element, one other overlay elem
        ds.insert_empty(Tag::new(0x6000, 0x3000), Vr::Ow);
        ds.insert_empty(Tag::new(0x6002, 0x3000), Vr::Ow);
        ds.insert_empty(Tag::new(0x6002, 0x0010), Vr::Us);

        let group: Vec<_> = ds.enumerate_group(0x0010).map(|i| i.tag().clone()).collect();
        assert_eq!(group, vec![PATIENT_NAME, PATIENT_ID], "group length (0000) excluded");

        let overlay = MaskedTag::parse("(60xx,3000)").unwrap();
        let masked: Vec<_> = ds.enumerate_masked(&overlay).map(|i| i.tag().clone()).collect();
        assert_eq!(masked, vec![Tag::new(0x6000, 0x3000), Tag::new(0x6002, 0x3000)]);

        assert_eq!(ds.enumerate_group(0x7FE0).count(), 0);
    }

    #[test]
    fn dataset_validate_is_on_demand_and_recursive() {
        let mut ds = Dataset::new();
        ds.insert_str(PATIENT_ID, Vr::Lo, "PID-1");
        // a valid dataset passes
        ds.validate().expect("valid dataset");

        // an invalid AE (>16 chars) fails with a ValidationFailure naming the tag
        ds.insert_str(Tag::ACCESSION_NUMBER, Vr::Ae, "SEVENTEEN_CHARS_X");
        let err = ds.validate().unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "{err}");
        assert!(err.to_string().contains("(0008,0050)"), "{err}");

        // recursion: a bad value inside a sequence item is caught
        let mut inner = Dataset::new();
        inner.insert_str(Tag::ACCESSION_NUMBER, Vr::Ae, "SEVENTEEN_CHARS_X");
        let mut outer = Dataset::new();
        let seq = outer.insert_sequence(Tag::REFERENCED_STUDY_SEQUENCE);
        seq.push(inner);
        assert!(outer.validate().is_err(), "invalid nested item is detected");

        // the per-dataset switch disables validation entirely
        let mut off = Dataset::new();
        off.set_validation(false);
        off.insert_str(Tag::ACCESSION_NUMBER, Vr::Ae, "SEVENTEEN_CHARS_X");
        off.validate().expect("validation disabled ignores bad values");
    }

    #[test]
    fn wrong_type_is_error_and_none() {
        let mut ds = Dataset::new();
        ds.insert_str(PATIENT_ID, Vr::Lo, "abc");
        assert!(ds.get::<u32>(PATIENT_ID).is_err());
        assert_eq!(ds.try_get::<u32>(PATIENT_ID), None);
        assert_eq!(ds.get_or(PATIENT_ID, 42_u32), 42);
    }

    #[test]
    fn insert_dispatch_matrix() {
        let mut ds = Dataset::new();
        ds.insert_value(PATIENT_NAME, "Doe^John");
        ds.insert_value(Tag::STUDY_INSTANCE_UID, crate::model::uid::Uid::new("1.2.3"));
        ds.insert_value(ROWS, 512_i32);
        ds.insert_value(Tag::SLICE_THICKNESS, 2.5_f64); // slice thickness DS
        ds.insert_value(Tag::PIXEL_DATA, vec![1u8, 2, 3]);

        let name = ds.get_element(PATIENT_NAME).unwrap();
        assert_eq!(name.vr(), Vr::Pn, "dictionary VR for (0010,0010)");
        let uid_el = ds.get_element(Tag::STUDY_INSTANCE_UID).unwrap();
        assert_eq!(uid_el.vr(), Vr::Ui);
        let rows_el = ds.get_element(ROWS).unwrap();
        assert_eq!(rows_el.vr(), Vr::Us, "dictionary VR for (0028,0010) must win over the IS value hint");
        let thick = ds.get_element(Tag::SLICE_THICKNESS).unwrap();
        assert_eq!(thick.vr(), Vr::Ds);
        let th: f64 = ds.get(Tag::SLICE_THICKNESS).unwrap();
        assert_eq!(th, 2.5);
        let pixels: SharedBuffer = ds.get(Tag::PIXEL_DATA).unwrap();
        assert_eq!(&*pixels.data(), &[1, 2, 3]);
    }

    /// The dictionary VR must win over a numeric value hint. Writing a `u16` to
    /// ROWS used to yield IS (a string VR), producing non-conformant bytes that
    /// only round-tripped because the reader shared the same mistake.
    #[test]
    fn dictionary_vr_beats_the_numeric_value_hint() {
        let mut ds = Dataset::new();
        ds.insert_value(ROWS, 512u16);
        ds.insert_value(COLUMNS, 256u16);
        ds.insert_value(Tag::NUMBER_OF_FRAMES, 3u16);
        ds.insert_value(Tag::RESCALE_INTERCEPT, 1.5f64);

        assert_eq!(ds.get_element(ROWS).unwrap().vr(), Vr::Us, "ROWS is US");
        assert_eq!(ds.get_element(COLUMNS).unwrap().vr(), Vr::Us, "COLUMNS is US");
        assert_eq!(
            ds.get_element(Tag::NUMBER_OF_FRAMES).unwrap().vr(),
            Vr::Is,
            "NumberOfFrames is IS, so IS stays IS"
        );
        assert_eq!(
            ds.get_element(Tag::RESCALE_INTERCEPT).unwrap().vr(),
            Vr::Ds,
            "RescaleIntercept is DS"
        );

        // Values survive the VR switch in both directions.
        assert_eq!(ds.get::<u16>(ROWS).unwrap(), 512);
        assert_eq!(ds.get::<u32>(COLUMNS).unwrap(), 256, "widening read of US");
        assert_eq!(ds.get::<f64>(Tag::RESCALE_INTERCEPT).unwrap(), 1.5);

        // And the wire bytes carry US, not IS.
        let file = crate::io::file::DicomFile::from_dataset(ds);
        let bytes = file.to_bytes(&crate::io::writer::WriteOptions::default()).unwrap();
        let pos = bytes
            .windows(4)
            .position(|w| w == [0x28, 0x00, 0x10, 0x00])
            .expect("ROWS element present");
        assert_eq!(&bytes[pos + 4..pos + 6], b"US", "encoded VR is US");
    }

    /// A hint that cannot be re-encoded must be preserved. An explicit VR on a
    /// binary-only tag (e.g. UN for a private tag) is the caller's decision and
    /// must not be silently rewritten.
    #[test]
    fn an_explicit_vr_on_a_binary_tag_is_preserved() {
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vec![1u8, 2, 3])),
        ));
        assert_eq!(ds.get_element(Tag::PIXEL_DATA).unwrap().vr(), Vr::Ob);
    }

    /// An unknown (private/unregistered) tag has no dictionary entry, so the
    /// value's own preference wins; with no preference it falls back to UN.
    #[test]
    fn unknown_tags_keep_the_value_preference() {
        let mut ds = Dataset::new();
        let private = Tag::new(0x0009, 0x1001);
        ds.insert_value(private.clone(), 7u16);
        assert_eq!(
            ds.get_element(&private).unwrap().vr(),
            Vr::Is,
            "no dictionary entry → the integer hint (IS) is kept"
        );
    }

    /// Every numeric VR may be written with the IS hint and read back.
    #[test]
    fn numeric_hint_covers_every_numeric_vr() {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::ROWS, 7u16);                        // US
        ds.insert_value(Tag::NUMBER_OF_FRAMES, 3u16);            // IS
        ds.insert_value(Tag::SMALLEST_IMAGE_PIXEL_VALUE, -5i32); // SS-capable dictionary VR
        ds.insert_value(Tag::RESCALE_INTERCEPT, -1.25f64);       // DS

        assert_eq!(ds.get_element(ROWS).unwrap().vr(), Vr::Us);
        assert_eq!(ds.get_element(Tag::NUMBER_OF_FRAMES).unwrap().vr(), Vr::Is);
        assert_eq!(ds.get_element(Tag::RESCALE_INTERCEPT).unwrap().vr(), Vr::Ds);

        // The SS case keeps its sign through the IS hint.
        let smallest = ds.get_element(Tag::SMALLEST_IMAGE_PIXEL_VALUE).unwrap().vr();
        assert!(matches!(smallest, Vr::Ss | Vr::Us | Vr::Is), "got {smallest:?}");
        assert_eq!(ds.get::<i32>(Tag::SMALLEST_IMAGE_PIXEL_VALUE).unwrap(), -5);
    }

    /// The encoded bytes must carry the dictionary VR, and re-parsing must
    /// recover it. This is the end-to-end guard against writing ROWS as IS.
    #[test]
    fn numeric_vr_survives_a_byte_round_trip() {
        use crate::io::writer::{DicomWriter, WriteOptions};
        let syntax = super::super::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let mut ds = Dataset::new();
        ds.set_transfer_syntax(syntax.clone());
        ds.insert_value(Tag::ROWS, 512u16);
        ds.insert_value(Tag::COLUMNS, 256u16);

        let mut out = Vec::new();
        DicomWriter::new(WriteOptions::default())
            .write_dataset(&ds, &syntax, &mut out)
            .unwrap();

        // Raw bytes carry "US", not "IS".
        let pos = out
            .windows(4)
            .position(|w| w == [0x28, 0x00, 0x10, 0x00])
            .expect("ROWS element present");
        assert_eq!(&out[pos + 4..pos + 6], b"US", "encoded VR is US");

        // And the decoder reads it back as US with the right value.
        let reparsed = crate::io::dataset_builder::parse_dataset_bytes(&out, &syntax).unwrap();
        assert_eq!(reparsed.get_element(ROWS).unwrap().vr(), Vr::Us);
        assert_eq!(reparsed.get::<u16>(ROWS).unwrap(), 512);
        assert_eq!(reparsed.get::<u16>(COLUMNS).unwrap(), 256);
    }

    #[test]
    fn insert_add_vs_update() {
        let mut ds = Dataset::new();
        ds.insert_str(PATIENT_ID, Vr::Lo, "A");
        assert!(ds.insert_new(PATIENT_ID, "B").is_err(), "Add duplicates error");
        ds.insert_str(PATIENT_ID, Vr::Lo, "B");
        assert_eq!(ds.get_str(PATIENT_ID).unwrap(), "B", "update replaces");
        assert_eq!(ds.value_count(PATIENT_ID).unwrap(), 1);
    }

    #[test]
    fn sequences_nested_and_counts() {
        let mut ds = Dataset::new();
        {
            let seq = ds.insert_sequence(Tag::REFERENCED_IMAGE_SEQUENCE);
            let mut item = Dataset::new();
            item.insert_str(Tag::REFERENCED_SOP_CLASS_UID, Vr::Ui, "1.2.3");
            seq.push(item);
            seq.push(Dataset::new());
        }
        let seq = ds.get_sequence(Tag::REFERENCED_IMAGE_SEQUENCE).unwrap();
        assert_eq!(seq.len(), 2);
        assert_eq!(ds.value_count(Tag::REFERENCED_IMAGE_SEQUENCE).unwrap(), 2);
        assert_eq!(
            seq.items()[0].get_str(Tag::REFERENCED_SOP_CLASS_UID).unwrap(),
            "1.2.3",
            "nested dataset reads"
        );
        assert!(ds.get::<String>(Tag::REFERENCED_IMAGE_SEQUENCE).is_err(), "SQ is not an element");
        // cascade: setting the outer syntax reaches nested items
        ds.set_transfer_syntax(syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone());
        let nested_ts = ds.get_sequence(Tag::REFERENCED_IMAGE_SEQUENCE).unwrap().items()[0].transfer_syntax();
        assert_eq!(nested_ts.uid(), "1.2.840.10008.1.2");
    }

    #[test]
    fn private_tag_resolution_and_creation() {
        let mut ds = Dataset::new();
        let creator = Arc::new(crate::model::private_creator::PrivateCreator::new("ACME 1.2"));
        let dict_style = Tag::private(0x0029, 0x0001, creator.clone());

        let resolved = ds.get_private_tag(dict_style.clone(), true).expect("block allocated");
        assert_eq!(resolved.group(), 0x0029);
        assert_eq!(resolved.element() & 0xFF, 0x01);
        assert_eq!(resolved.element() >> 8, 0x10, "first free block");

        // creator element materialized at (0029,0010)
        assert_eq!(ds.get_str(Tag::new(0x0029, 0x0010)).unwrap(), "ACME 1.2");

        // resolution now finds the same block
        let again = ds.get_private_tag(dict_style.clone(), false).unwrap();
        assert_eq!(again, resolved);

        // store a value at the block tag and read it back through the
        // dictionary-style tag
        ds.insert_str(resolved.clone(), Vr::Lo, "widget");
        assert_eq!(ds.get_str(dict_style).unwrap(), "widget", "block-relative reads");

        // non-private and block tags pass through unchanged
        assert_eq!(ds.get_private_tag(Tag::PATIENT_NAME, false), Some(Tag::PATIENT_NAME));
    }

    #[test]
    fn insert_or_update_resolves_and_retags_new_private_creator() {
        let mut ds = Dataset::new();
        let creator = Arc::new(crate::model::private_creator::PrivateCreator::new("ACME 1.2"));
        let dict_style = Tag::private(0x0029, 0x0001, creator.clone());

        ds.insert_or_update(DicomElement::new_str(dict_style.clone(), Vr::Lo, "widget"));

        let resolved = ds.get_private_tag(dict_style.clone(), false).expect("creator block allocated");
        assert_eq!(resolved.element(), 0x1001);
        assert_eq!(ds.get_str(dict_style).unwrap(), "widget");
        assert!(ds.items().any(|item| item.tag() == &resolved));
        assert!(ds.items().all(|item| item.tag().element() != 0x0001));

        let sequence_tag = Tag::private(0x0029, 0x0002, creator.clone());
        ds.insert_or_update(DicomItem::Sequence(DicomSequence::new(sequence_tag.clone())));
        assert_eq!(ds.get_sequence(sequence_tag).unwrap().tag().element(), 0x1002);

        let fragments_tag = Tag::private(0x0029, 0x0003, creator);
        ds.insert_or_update(DicomItem::Fragments(DicomFragmentSequence::new(
            fragments_tag.clone(),
            Vr::Ob,
        )));
        assert_eq!(ds.item(&fragments_tag).unwrap().tag().element(), 0x1003);
    }

    #[test]
    fn private_group_exhaustion() {
        let mut ds = Dataset::new();
        let mk = |name: &str| Tag::private(0x002B, 0x0001, Arc::new(crate::model::private_creator::PrivateCreator::new(name)));
        // fill all 240 blocks (0x10..=0xFF)
        for i in 0..240u32 {
            let tag = mk(&format!("C{i}"));
            let got = ds.get_private_tag(tag, true).expect("block available");
            assert!(got.element() >= 0x1000);
        }
        let overflow = mk("one more");
        assert_eq!(ds.get_private_tag(overflow, true), None, "group exhausted");
    }

    #[test]
    fn copy_clone_and_retain() {
        let mut ds = Dataset::new();
        ds.insert_str(PATIENT_ID, Vr::Lo, "X");
        ds.insert_str(PATIENT_NAME, Vr::Pn, "Doe^John");

        let mut dest = Dataset::new();
        ds.copy_to(&mut dest);
        assert_eq!(dest.len(), 2);
        assert_eq!(dest.get_str(PATIENT_ID).unwrap(), "X");

        let mut cloned = ds.clone();
        cloned.insert_str(ROWS, Vr::Is, "1");
        assert_eq!(ds.len(), 2, "deep clone does not affect the source");

        ds.retain(|item| item.tag() != &PATIENT_NAME);
        assert_eq!(ds.len(), 1);
        ds.clear();
        assert!(ds.is_empty());
    }

    #[test]
    fn walker_traverses_in_tag_order() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds.insert_str(Tag::ACCESSION_NUMBER, Vr::Sh, "ACC1");
        {
            let seq = ds.insert_sequence(Tag::REFERENCED_IMAGE_SEQUENCE);
            let mut item = Dataset::new();
            item.insert_str(Tag::REFERENCED_SOP_CLASS_UID, Vr::Ui, "1.2.3");
            seq.push(item);
        }
        ds.insert_value(Tag::PIXEL_DATA, vec![1u8, 2]);
        ds.insert_fragments(Tag::FLOAT_PIXEL_DATA, Vr::Ob)
            .push_fragment(Arc::new(MemoryBuffer::from(vec![9, 9])));

        #[derive(Default)]
        struct Recorder {
            log: Vec<String>,
        }
        impl DatasetWalker for Recorder {
            fn on_element(&mut self, e: &DicomElement) {
                self.log.push(format!("el {}", e.tag()));
            }
            fn on_sequence_begin(&mut self, s: &DicomSequence) {
                self.log.push(format!("seq+ {}", s.tag()));
            }
            fn on_sequence_item_begin(&mut self, _item: &Dataset) {
                self.log.push("item+".into());
            }
            fn on_sequence_item_end(&mut self, _item: &Dataset) {
                self.log.push("item-".into());
            }
            fn on_sequence_end(&mut self, s: &DicomSequence) {
                self.log.push(format!("seq- {}", s.tag()));
            }
            fn on_fragments_begin(&mut self, f: &DicomFragmentSequence) {
                self.log.push(format!("frag+ {}", f.tag()));
            }
            fn on_fragment(&mut self, _f: &DicomFragmentSequence, b: &SharedBuffer) {
                self.log.push(format!("frag {} bytes", b.size()));
            }
            fn on_fragments_end(&mut self, f: &DicomFragmentSequence) {
                self.log.push(format!("frag- {}", f.tag()));
            }
        }

        let mut w = Recorder::default();
        ds.walk(&mut w);
        assert_eq!(
            w.log,
            vec![
                "el (0008,0050)",
                "seq+ (0008,1140)",
                "item+",
                "el (0008,1150)",
                "item-",
                "seq- (0008,1140)",
                "el (0010,0010)",
                "frag+ (7FE0,0008)",
                "frag 2 bytes",
                "frag- (7FE0,0008)",
                "el (7FE0,0010)",
            ],
            "depth-first, tag-ascending traversal"
        );
    }

    #[test]
    fn empty_insert_and_raw_reads() {
        let mut ds = Dataset::new();
        ds.insert_empty(Tag::PATIENT_WEIGHT, Vr::Ds);
        assert_eq!(ds.value_count(Tag::PATIENT_WEIGHT).unwrap(), 0);
        assert_eq!(ds.try_get_str(Tag::PATIENT_WEIGHT).unwrap(), "");

        let mut src = Dataset::new();
        src.insert_value(Tag::NUMBER_OF_FRAMES, vec![9u8]);
        let buf: SharedBuffer = src.get(Tag::NUMBER_OF_FRAMES).unwrap();
        assert_eq!(buf.size(), 1);
    }
}

// -- derived operations (DS-06, DS-07, DS-08) -------------------------------

impl PartialEq for Dataset {
    /// Deep content equality: same item count and every tag's item equal
    /// (recursive through sequences). Transfer syntax and validation flags
    /// are not part of content.
    fn eq(&self, other: &Self) -> bool {
        self.items.len() == other.items.len()
            && self.items.iter().zip(other.items.iter()).all(|((ta, ia), (tb, ib))| ta == tb && ia == ib)
    }
}

impl Eq for Dataset {}

impl Dataset {
    /// Content comparison matching fo-dicom `DicomDatasetComparer`.
    ///
    /// Same as [`PartialEq`] for tags, VRs and values (recursing through
    /// sequences), except that an element backed by an unresolved bulk-data
    /// URI is compared by tag alone: its bytes have not been fetched, so
    /// comparing them would be meaningless (fo-dicom `DicomValueComparer`
    /// skips a `BulkDataUriByteBuffer` that is not in memory).
    pub fn content_equals(&self, other: &Self) -> bool {
        self.items.len() == other.items.len()
            && self.items.iter().all(|(tag, item)| {
                other.items.get(tag).is_some_and(|other_item| items_content_equal(item, other_item))
            })
    }
}

fn items_content_equal(a: &DicomItem, b: &DicomItem) -> bool {
    match (a, b) {
        (DicomItem::Element(x), DicomItem::Element(y)) => {
            if x.tag() != y.tag() || x.vr() != y.vr() {
                return false;
            }
            if skips_unresolved_bulk(x) || skips_unresolved_bulk(y) {
                return true;
            }
            x.value() == y.value()
        }
        (DicomItem::Sequence(x), DicomItem::Sequence(y)) => {
            x.tag() == y.tag()
                && x.items().len() == y.items().len()
                && x.items().iter().zip(y.items()).all(|(d1, d2)| d1.content_equals(d2))
        }
        (DicomItem::Fragments(x), DicomItem::Fragments(y)) => x == y,
        _ => false,
    }
}

fn skips_unresolved_bulk(element: &DicomElement) -> bool {
    element
        .value()
        .as_buffer()
        .is_some_and(|buffer| buffer.bulk_uri().is_some() && !buffer.is_memory())
}

/// Shared functional groups sequence (0020,9116).
pub const SHARED_FUNCTIONAL_GROUPS: Tag = Tag::PLANE_ORIENTATION_SEQUENCE;
/// Per-frame functional groups sequence (0020,9111).
pub const PER_FRAME_FUNCTIONAL_GROUPS: Tag = Tag::FRAME_CONTENT_SEQUENCE;
/// Timezone offset from UTC (0008,0201), `±hhmm`.
pub const TIMEZONE_OFFSET_FROM_UTC: Tag = Tag::TIMEZONE_OFFSET_FROM_UTC;

impl Dataset {
    /// Flattens enhanced-multiframe functional groups for one frame into a
    /// transient dataset (DS-06): SharedFunctionalGroups items first, then
    /// the PerFrameFunctionalGroups item for `frame`. Empty sequences are
    /// skipped; other items copy whole with insert_or_update semantics.
    pub fn functional_groups(&self, frame: u32) -> Result<Dataset, Error> {
        let mut out = Dataset::with_transfer_syntax(self.transfer_syntax.clone());
        fn push_source(out: &mut Dataset, source: &Dataset) {
            for item in source.items() {
                if let DicomItem::Sequence(seq) = item {
                    if seq.is_empty() {
                        continue; // skip empty functional group macros
                    }
                }
                out.insert_or_update(item.clone());
            }
        }
        if let Some(shared) = self.try_get_sequence(SHARED_FUNCTIONAL_GROUPS) {
            if let Some(first) = shared.items().first() {
                push_source(&mut out, first);
            }
        }
        if let Some(per_frame) = self.try_get_sequence(PER_FRAME_FUNCTIONAL_GROUPS) {
            let item = per_frame.items().get(frame as usize).ok_or_else(|| {
                Error::Data(DataError::IndexOutOfRange {
                    tag: TagRef(PER_FRAME_FUNCTIONAL_GROUPS.group(), PER_FRAME_FUNCTIONAL_GROUPS.element()),
                    index: frame as usize,
                    count: per_frame.len(),
                })
            })?;
            push_source(&mut out, item);
        }
        Ok(out)
    }

    /// Composes a naive date-time from separate DA and TM elements (DS-08);
    /// a missing time defaults to midnight.
    pub fn get_datetime(
        &self,
        date_tag: impl AsRef<Tag>,
        time_tag: impl AsRef<Tag>,
    ) -> Result<chrono::NaiveDateTime, Error> {
        let date: chrono::NaiveDate = self.get(date_tag)?;
        let time: chrono::NaiveTime = self.get_or(time_tag.as_ref().clone(), chrono::NaiveTime::MIN);
        Ok(chrono::NaiveDateTime::new(date, time))
    }

    pub fn try_get_datetime(
        &self,
        date_tag: impl AsRef<Tag>,
        time_tag: impl AsRef<Tag>,
    ) -> Option<chrono::NaiveDateTime> {
        self.get_datetime(date_tag, time_tag).ok()
    }

    /// Composes a timezone-aware date-time, reading
    /// TimezoneOffsetFromUTC (0008,0201, `±hhmm`); without it the value is
    /// presented as UTC.
    pub fn get_datetime_with_offset(
        &self,
        date_tag: impl AsRef<Tag>,
        time_tag: impl AsRef<Tag>,
    ) -> Result<chrono::DateTime<chrono::FixedOffset>, Error> {
        let naive = self.get_datetime(date_tag, time_tag)?;
        let offset = match self.try_get_str(TIMEZONE_OFFSET_FROM_UTC) {
            Some(tz) => {
                let t = tz.trim();
                let invalid = || {
                    Error::Data(DataError::Other {
                        message: format!("invalid TimezoneOffsetFromUTC \"{tz}\""),
                    })
                };
                let bytes = t.as_bytes();
                if t.len() != 5 || !matches!(bytes[0], b'+' | b'-') {
                    return Err(invalid());
                }
                let sign = if bytes[0] == b'-' { -1 } else { 1 };
                let h: i32 = t[1..3].parse().map_err(|_| invalid())?;
                let m: i32 = t[3..5].parse().map_err(|_| invalid())?;
                let secs = sign * (h * 3600 + m * 60);
                chrono::FixedOffset::east_opt(secs).ok_or_else(invalid)?
            }
            None => chrono::FixedOffset::east_opt(0).expect("zero offset"),
        };
        use chrono::TimeZone;
        // the naive value is local time AT the offset (DICOM DT semantics)
        offset
            .from_local_datetime(&naive)
            .single()
            .ok_or_else(|| Error::Data(DataError::Other {
                message: "date-time does not exist in the given offset".into(),
            }))
    }
}

#[cfg(test)]
mod derived_tests {
    use super::*;
    const PATIENT_ID: Tag = Tag::PATIENT_ID;

    fn enhanced_dataset() -> Dataset {
        let mut ds = Dataset::new();
        {
            let shared = ds.insert_sequence(SHARED_FUNCTIONAL_GROUPS);
            let mut item = Dataset::new();
            item.insert_str(Tag::SLICE_THICKNESS, Vr::Ds, "2.5"); // SliceThickness
            item.insert_sequence(Tag::PLANE_POSITION_SEQUENCE); // empty sequence: skipped
            shared.push(item);
        }
        {
            let per = ds.insert_sequence(PER_FRAME_FUNCTIONAL_GROUPS);
            let mut f0 = Dataset::new();
            f0.insert_str(Tag::SLICE_LOCATION, Vr::Ds, "-5.0"); // SliceLocation
            per.push(f0);
            let mut f1 = Dataset::new();
            f1.insert_str(Tag::SLICE_LOCATION, Vr::Ds, "-2.5");
            per.push(f1);
        }
        ds
    }

    #[test]
    fn functional_groups_flatten() {
        let ds = enhanced_dataset();
        let f0 = ds.functional_groups(0).unwrap();
        assert_eq!(f0.get_str(Tag::SLICE_THICKNESS).unwrap(), "2.5", "shared contributes");
        assert_eq!(f0.get_str(Tag::SLICE_LOCATION).unwrap(), "-5.0", "frame 0 location");
        let f1 = ds.functional_groups(1).unwrap();
        assert_eq!(f1.get_str(Tag::SLICE_LOCATION).unwrap(), "-2.5");

        // empty sequences are not carried over
        assert!(f0.try_get_sequence(Tag::PLANE_POSITION_SEQUENCE).is_none());

        // out-of-range frame errors
        assert!(ds.functional_groups(7).is_err());
    }

    #[test]
    fn content_equality_is_deep() {
        let mut a = Dataset::new();
        a.insert_str(PATIENT_ID, Vr::Lo, "X");
        {
            let seq = a.insert_sequence(Tag::REFERENCED_IMAGE_SEQUENCE);
            let mut item = Dataset::new();
            item.insert_str(Tag::REFERENCED_SOP_CLASS_UID, Vr::Ui, "1.2.3");
            seq.push(item);
        }
        let mut b = Dataset::new();
        b.insert_str(PATIENT_ID, Vr::Lo, "X");
        {
            let seq = b.insert_sequence(Tag::REFERENCED_IMAGE_SEQUENCE);
            let mut item = Dataset::new();
            item.insert_str(Tag::REFERENCED_SOP_CLASS_UID, Vr::Ui, "1.2.3");
            seq.push(item);
        }
        assert_eq!(a, b, "identical content compares equal");

        // different transfer syntax does not affect content equality
        b.set_transfer_syntax(syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone());
        assert_eq!(a, b);

        // a value difference breaks it
        b.insert_str(PATIENT_ID, Vr::Lo, "Y");
        assert_ne!(a, b);
    }

    #[test]
    fn content_comparison_skips_unresolved_bulk_data() {
        use crate::model::buffer::BulkDataUri;

        let unresolved = |uri: &str| -> SharedBuffer { Arc::new(BulkDataUri::new(uri)) };
        let resolved = |data: Vec<u8>| -> SharedBuffer {
            Arc::new(BulkDataUri::with_data("http://example/bulk", data))
        };

        let mut a = Dataset::new();
        a.insert(DicomElement::new_bytes(Tag::PIXEL_DATA, Vr::Ob, unresolved("http://a/1"))).unwrap();
        let mut b = Dataset::new();
        b.insert(DicomElement::new_bytes(Tag::PIXEL_DATA, Vr::Ob, unresolved("http://b/2"))).unwrap();
        assert!(a.content_equals(&b), "unresolved bulk data is compared by tag only");

        let mut c = Dataset::new();
        c.insert(DicomElement::new_bytes(Tag::PIXEL_DATA, Vr::Ob, resolved(vec![1, 2, 3]))).unwrap();
        let mut d = Dataset::new();
        d.insert(DicomElement::new_bytes(Tag::PIXEL_DATA, Vr::Ob, resolved(vec![1, 2, 4]))).unwrap();
        assert!(!c.content_equals(&d), "materialized bulk data still compares bytes");

        // tag set must match exactly
        let mut e = Dataset::new();
        e.insert_str(PATIENT_ID, Vr::Lo, "X");
        assert!(!a.content_equals(&e));
    }

    #[test]
    fn datetime_composition() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::ACQUISITION_DATE, Vr::Da, "20240305"); // AcquisitionDate
        ds.insert_str(Tag::ACQUISITION_TIME, Vr::Tm, "103015"); // AcquisitionTime

        let dt = ds
            .get_datetime(Tag::ACQUISITION_DATE, Tag::ACQUISITION_TIME)
            .unwrap();
        assert_eq!(dt.to_string(), "2024-03-05 10:30:15");

        // missing time -> midnight
        let mut no_time = Dataset::new();
        no_time.insert_str(Tag::ACQUISITION_DATE, Vr::Da, "20240305");
        let dt = no_time
            .get_datetime(Tag::ACQUISITION_DATE, Tag::ACQUISITION_TIME)
            .unwrap();
        assert_eq!(dt.to_string(), "2024-03-05 00:00:00");

        // with timezone offset
        ds.insert_str(TIMEZONE_OFFSET_FROM_UTC, Vr::Sh, "+0530");
        let tz = ds
            .get_datetime_with_offset(Tag::ACQUISITION_DATE, Tag::ACQUISITION_TIME)
            .unwrap();
        assert_eq!(tz.offset().local_minus_utc(), 5 * 3600 + 30 * 60);
        assert_eq!(tz.to_rfc3339(), "2024-03-05T10:30:15+05:30");

        // invalid offset errors
        ds.insert_str(TIMEZONE_OFFSET_FROM_UTC, Vr::Sh, "0530");
        assert!(ds
            .get_datetime_with_offset(Tag::ACQUISITION_DATE, Tag::ACQUISITION_TIME)
            .is_err());
    }
}
