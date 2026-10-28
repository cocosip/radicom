//! Dataset items: elements, sequences and fragment sequences
//! (ELEM-03, ELEM-04; docs/01-architecture.md §3).

use crate::model::buffer::SharedBuffer;
use crate::model::dataset::Dataset;
use crate::model::element::DicomElement;
use crate::model::tag::Tag;
use crate::model::vr::Vr;

/// One stored item in a dataset: an element, a sequence, or an
/// encapsulated-pixel-data fragment sequence.
#[derive(Debug, Clone)]
pub enum DicomItem {
    Element(DicomElement),
    Sequence(DicomSequence),
    Fragments(DicomFragmentSequence),
}

/// A SQ item container (ELEM-04): ordered list of datasets; recursive
/// validation happens at the dataset layer.
#[derive(Debug, Clone)]
pub struct DicomSequence {
    tag: Tag,
    items: Vec<Dataset>,
}

impl DicomSequence {
    pub fn new(tag: Tag) -> Self {
        DicomSequence { tag, items: Vec::new() }
    }

    pub fn with_items(tag: Tag, items: Vec<Dataset>) -> Self {
        DicomSequence { tag, items }
    }

    pub fn tag(&self) -> &Tag {
        &self.tag
    }

    pub fn items(&self) -> &[Dataset] {
        &self.items
    }

    pub fn items_mut(&mut self) -> &mut Vec<Dataset> {
        &mut self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn push(&mut self, item: Dataset) {
        self.items.push(item);
    }
}

/// Encapsulated pixel data (ELEM-03): the basic offset table plus
/// fragment buffers. Fragment 0 in the store is the offset table by
/// convention; the API keeps it as a typed field instead.
#[derive(Debug, Clone)]
pub struct DicomFragmentSequence {
    tag: Tag,
    vr: Vr, // OB or OW
    offset_table: Vec<u32>,
    fragments: Vec<SharedBuffer>,
    /// Running byte offset of the next fragment's item tag, maintained
    /// incrementally so `push_fragment` stays O(1) per fragment.
    next_offset: u64,
}

impl DicomFragmentSequence {
    pub fn new(tag: Tag, vr: Vr) -> Self {
        debug_assert!(matches!(vr, Vr::Ob | Vr::Ow), "fragments are OB/OW only");
        DicomFragmentSequence {
            tag,
            vr,
            offset_table: Vec::new(),
            fragments: Vec::new(),
            next_offset: 0,
        }
    }

    pub fn tag(&self) -> &Tag {
        &self.tag
    }

    pub fn vr(&self) -> Vr {
        self.vr
    }

    /// The basic offset table (byte offsets relative to the first
    /// fragment's first byte).
    pub fn offset_table(&self) -> &[u32] {
        &self.offset_table
    }

    pub fn fragments(&self) -> &[SharedBuffer] {
        &self.fragments
    }

    /// Appends a fragment and maintains the basic offset table: each entry is
    /// the byte distance from the first fragment's item tag to this
    /// fragment's item tag, i.e. every preceding fragment contributes its
    /// 8-byte item header plus its padded length (docs/01-architecture.md §7).
    /// A computed offset beyond `u32::MAX` clears the table (readers then
    /// fall back to the fragment-count == frame-count mapping, spec 06 §1.4).
    pub fn push_fragment(&mut self, fragment: SharedBuffer) {
        if self.next_offset <= u32::MAX as u64 {
            self.offset_table.push(self.next_offset as u32);
        } else {
            self.offset_table.clear();
        }
        self.next_offset += 8 + fragment.size() + fragment.size() % 2;
        self.fragments.push(fragment);
    }

    /// Appends a fragment without touching the offset table. The parser uses
    /// this: the wire carries its own basic offset table, which
    /// [`set_offset_table`](Self::set_offset_table) installs afterwards.
    pub fn push_fragment_raw(&mut self, fragment: SharedBuffer) {
        self.fragments.push(fragment);
    }

    pub fn set_offset_table(&mut self, table: Vec<u32>) {
        self.offset_table = table;
        // re-sync the incremental counter with whatever was installed
        self.next_offset = self.fragments.iter().map(|f| 8 + f.size() + f.size() % 2).sum();
    }


}

impl DicomItem {
    pub fn tag(&self) -> &Tag {
        match self {
            DicomItem::Element(e) => e.tag(),
            DicomItem::Sequence(s) => s.tag(),
            DicomItem::Fragments(f) => f.tag(),
        }
    }

    pub(crate) fn with_tag(self, tag: Tag) -> Self {
        match self {
            DicomItem::Element(element) => DicomItem::Element(element.with_tag(tag)),
            DicomItem::Sequence(mut sequence) => {
                sequence.tag = tag;
                DicomItem::Sequence(sequence)
            }
            DicomItem::Fragments(mut fragments) => {
                fragments.tag = tag;
                DicomItem::Fragments(fragments)
            }
        }
    }

    pub fn vr(&self) -> Vr {
        match self {
            DicomItem::Element(e) => e.vr(),
            DicomItem::Sequence(_) => Vr::Sq,
            DicomItem::Fragments(f) => f.vr(),
        }
    }

    pub fn is_empty_value(&self) -> bool {
        match self {
            DicomItem::Element(e) => e.is_empty(),
            DicomItem::Sequence(s) => s.is_empty(),
            DicomItem::Fragments(f) => f.fragments().iter().all(|b| b.size() == 0),
        }
    }
}


impl PartialEq for DicomSequence {
    fn eq(&self, other: &Self) -> bool {
        self.tag == other.tag && self.items == other.items
    }
}
impl Eq for DicomSequence {}

impl PartialEq for DicomFragmentSequence {
    /// Content equality: offset table and fragment bytes.
    fn eq(&self, other: &Self) -> bool {
        self.tag == other.tag
            && self.vr == other.vr
            && self.offset_table == other.offset_table
            && self.fragments.len() == other.fragments.len()
            && self
                .fragments
                .iter()
                .zip(other.fragments.iter())
                .all(|(a, b)| a.size() == b.size() && a.data() == b.data())
    }
}
impl Eq for DicomFragmentSequence {}

impl PartialEq for DicomItem {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (DicomItem::Element(a), DicomItem::Element(b)) => a == b,
            (DicomItem::Sequence(a), DicomItem::Sequence(b)) => a == b,
            (DicomItem::Fragments(a), DicomItem::Fragments(b)) => a == b,
            _ => false,
        }
    }
}
impl Eq for DicomItem {}

impl From<DicomElement> for DicomItem {
    fn from(e: DicomElement) -> Self {
        DicomItem::Element(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::buffer::MemoryBuffer;
use std::sync::Arc;

    #[test]
    fn sequence_container() {
        let mut seq = DicomSequence::new(Tag::REFERENCED_STUDY_SEQUENCE);
        seq.push(Dataset::new());
        seq.push(Dataset::new());
        assert_eq!(seq.len(), 2);
        assert_eq!(seq.tag(), &Tag::REFERENCED_STUDY_SEQUENCE);
        let item = DicomItem::Sequence(seq);
        assert_eq!(item.vr(), Vr::Sq);
    }

    #[test]
    fn fragment_offset_table_growth() {
        let mut f = DicomFragmentSequence::new(Tag::PIXEL_DATA, Vr::Ob);
        f.push_fragment(Arc::new(MemoryBuffer::from(vec![1, 2])));
        assert_eq!(f.offset_table(), &[0], "first fragment starts at 0");
        // each preceding fragment contributes its 8-byte item header plus its
        // (padded) length
        f.push_fragment(Arc::new(MemoryBuffer::from(vec![3, 4, 5])));
        assert_eq!(f.offset_table(), &[0, 10], "8-byte header + 2-byte value");
        f.push_fragment(Arc::new(MemoryBuffer::from(vec![6])));
        assert_eq!(f.offset_table(), &[0, 10, 22], "odd fragments pad to even");
        // manual construction (parser path) replaces tracking entirely
        f.set_offset_table(vec![7]);
        assert_eq!(f.offset_table(), &[7]);
    }

    #[test]
    fn item_vr_dispatch() {
        let e = DicomElement::new_str(Tag::PATIENT_NAME, Vr::Pn, "X");
        let item = DicomItem::Element(e);
        assert_eq!(item.vr(), Vr::Pn);
        assert!(!item.is_empty_value());
        let empty = DicomItem::Element(DicomElement::new_empty(Tag::PATIENT_ID, Vr::Lo));
        assert!(empty.is_empty_value());
    }
}
