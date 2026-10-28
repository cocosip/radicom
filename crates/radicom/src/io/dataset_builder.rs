//! Dataset-building parse observer (RD-02; docs/01-architecture.md §7).

use crate::encoding::charset::{charset_encoding, decode_multi_charset};
use crate::error::{DataError, Error, TagRef};
use crate::io::reader::{ParseContext, ParseObserver};
use crate::model::buffer::SharedBuffer;
use crate::model::element::DicomElement;
use crate::model::item::{DicomFragmentSequence, DicomItem, DicomSequence};
use crate::model::tag::Tag;
use crate::model::transfer_syntax::TransferSyntax;
use crate::model::vr::Vr;
use crate::Dataset;
use std::sync::Arc;

/// Builds a [`Dataset`] from parser events: keeps a dataset stack for
/// sequence nesting, tracks SpecificCharacterSet per level and rejects
/// fragment sequences with non-OB/OW VRs.
#[derive(Debug)]
pub struct DatasetBuilder {
    root: Dataset,
    /// Innermost-first dataset stack; empty means the root.
    dataset_stack: Vec<Dataset>,
    /// Open sequences, innermost last.
    sequence_stack: Vec<DicomSequence>,
    /// Open fragment sequences (no nesting inside them).
    fragment_stack: Vec<DicomFragmentSequence>,
    /// Basic offset table decoded from fragment 0, applied once all
    /// fragments are in (the wire table is authoritative, so it must not be
    /// recomputed by `push_fragment`).
    fragment_offset_table: Option<Vec<u32>>,
    /// Active charset per nesting level, parallel to the dataset stack.
    charset_stack: Vec<&'static encoding_rs::Encoding>,
}

impl Default for DatasetBuilder {
    fn default() -> Self {
        DatasetBuilder {
            root: Dataset::new(),
            dataset_stack: Vec::new(),
            sequence_stack: Vec::new(),
            fragment_stack: Vec::new(),
            fragment_offset_table: None,
            charset_stack: vec![charset_encoding("ISO_IR 6")],
        }
    }
}

impl DatasetBuilder {
    /// Builder for a dataset encoded with `syntax`.
    pub fn with_transfer_syntax(syntax: Arc<TransferSyntax>) -> Self {
        DatasetBuilder { root: Dataset::with_transfer_syntax(syntax), ..Default::default() }
    }

    pub fn new() -> Self {
        DatasetBuilder::default()
    }

    /// Takes the built root dataset.
    pub fn into_dataset(self) -> Dataset {
        self.root
    }

    fn current(&mut self) -> &mut Dataset {
        self.dataset_stack.last_mut().unwrap_or(&mut self.root)
    }

    fn current_charset(&self) -> &'static encoding_rs::Encoding {
        self.charset_stack.last().copied().unwrap_or_else(|| charset_encoding("ISO_IR 6"))
    }

    fn charset_of(value: &str) -> Option<&'static encoding_rs::Encoding> {
        // Specific Character Set is multi-valued; a leading empty value means the
        // default repertoire (ISO-IR 6). Use the first *non-empty* value as the
        // governing encoding so a list like `\ISO 2022 IR 58` resolves to GBK
        // rather than falling back to ASCII.
        let first = value.split('\\').map(str::trim).find(|term| !term.is_empty())?;
        Some(charset_encoding(first))
    }

    /// Decodes raw value bytes with the active charset (ENC-02).
    fn decode_str(&self, bytes: &[u8], pn: bool) -> String {
        decode_multi_charset(bytes, self.current_charset(), pn)
    }
}

impl ParseObserver for DatasetBuilder {
    fn on_element(&mut self, _ctx: &ParseContext, tag: &Tag, vr: Vr, buffer: SharedBuffer) -> Result<(), Error> {
        // SpecificCharacterSet: ASCII by definition, and it governs this
        // level plus nested items that do not override it
        if tag.group() == 0x0008 && tag.element() == 0x0005 {
            let text = String::from_utf8_lossy(&buffer.data()).trim_end_matches([' ', '\0']).to_string();
            let enc = Self::charset_of(&text).unwrap_or_else(|| charset_encoding("ISO_IR 6"));
            if let Some(top) = self.charset_stack.last_mut() {
                *top = enc;
            }
            self.current().insert_str(tag.clone(), Vr::Cs, text);
            return Ok(());
        }

        let element = if vr.is_string() {
            let text = self.decode_str(&buffer.data(), vr == Vr::Pn);
            DicomElement::new_str(tag.clone(), vr, trim_value_padding(vr, text))
        } else {
            DicomElement::new_bytes(tag.clone(), vr, buffer)
        };
        self.current().insert_or_update(DicomItem::Element(element));
        Ok(())
    }

    fn on_begin_sequence(&mut self, _ctx: &ParseContext, tag: &Tag, _defined_length: Option<u32>) -> Result<(), Error> {
        self.sequence_stack.push(DicomSequence::new(tag.clone()));
        Ok(())
    }

    fn on_begin_sequence_item(&mut self, _ctx: &ParseContext, _defined_length: Option<u32>) -> Result<(), Error> {
        let ds = Dataset::with_transfer_syntax(self.current().transfer_syntax());
        self.dataset_stack.push(ds);
        // nested items inherit the enclosing charset until they override it
        let inherited = self.current_charset();
        self.charset_stack.push(inherited);
        Ok(())
    }

    fn on_end_sequence_item(&mut self, _ctx: &ParseContext) -> Result<(), Error> {
        let item = self.dataset_stack.pop().ok_or_else(|| {
            Error::Data(DataError::Other { message: "sequence item stack underflow".into() })
        })?;
        if self.charset_stack.len() > 1 {
            self.charset_stack.pop();
        }
        if let Some(seq) = self.sequence_stack.last_mut() {
            seq.push(item);
        }
        Ok(())
    }

    fn on_end_sequence(&mut self, _ctx: &ParseContext) -> Result<(), Error> {
        let seq = self
            .sequence_stack
            .pop()
            .ok_or_else(|| Error::Data(DataError::Other { message: "sequence stack underflow".into() }))?;
        self.current().insert_or_update(DicomItem::Sequence(seq));
        Ok(())
    }

    fn on_begin_fragment(
        &mut self,
        _ctx: &ParseContext,
        tag: &Tag,
        vr: Vr,
        offset_table: Vec<u32>,
    ) -> Result<(), Error> {
        if !matches!(vr, Vr::Ob | Vr::Ow) {
            return Err(Error::Data(DataError::UnexpectedType {
                tag: TagRef(tag.group(), tag.element()),
                expected: "OB or OW",
                found: vr.code(),
            }));
        }
        self.fragment_offset_table = Some(offset_table);
        self.fragment_stack.push(DicomFragmentSequence::new(tag.clone(), vr));
        Ok(())
    }

    fn on_fragment(&mut self, _ctx: &ParseContext, buffer: SharedBuffer) -> Result<(), Error> {
        if let Some(f) = self.fragment_stack.last_mut() {
            // the wire's own offset table replaces any computed one
            f.push_fragment_raw(buffer);
        }
        Ok(())
    }

    fn on_end_fragment(&mut self, _ctx: &ParseContext) -> Result<(), Error> {
        let mut frags = self
            .fragment_stack
            .pop()
            .ok_or_else(|| Error::Data(DataError::Other { message: "fragment stack underflow".into() }))?;
        if let Some(table) = self.fragment_offset_table.take() {
            frags.set_offset_table(table);
        }
        self.current().insert_or_update(DicomItem::Fragments(frags));
        Ok(())
    }
}

/// Strips the value padding (PS3.5 §6.2): UI pads with NUL, string VRs with
/// a space. Without this a round-trip would grow the value on every write.
fn trim_value_padding(vr: Vr, mut text: String) -> String {
    let pad = vr.padding();
    while text.ends_with(pad as char) {
        text.pop();
    }
    text
}

/// Convenience: parse a byte slice into a dataset. Deflated transfer
/// syntaxes are inflated first (RD-04/T10).
pub fn parse_dataset_bytes(bytes: &[u8], syntax: &TransferSyntax) -> Result<Dataset, Error> {
    let owned;
    let bytes = if syntax.is_deflate() {
        owned = crate::io::reader::inflate(bytes)?;
        &owned[..]
    } else {
        bytes
    };
    let mut source =
        crate::io::byte_source::ByteSourceReader::new(std::io::Cursor::new(bytes.to_vec()), syntax.endian());
    let reader = crate::io::reader::DicomReader::default();
    let mut builder = DatasetBuilder::with_transfer_syntax(Arc::new(syntax.clone()));
    reader.read(&mut source, syntax, &mut builder, None)?;
    Ok(builder.into_dataset())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ReaderError;
    use crate::model::transfer_syntax::{syntaxes, TransferSyntax};

    fn le() -> Arc<TransferSyntax> {
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone()
    }

    fn implicit() -> Arc<TransferSyntax> {
        syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone()
    }

    fn el_header(group: u16, element: u16, vr: &str, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&group.to_le_bytes());
        out.extend_from_slice(&element.to_le_bytes());
        out.extend_from_slice(vr.as_bytes());
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        out.extend_from_slice(value);
        out
    }

    fn el_long(group: u16, element: u16, vr: &str, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&group.to_le_bytes());
        out.extend_from_slice(&element.to_le_bytes());
        out.extend_from_slice(vr.as_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
        out
    }

    fn item(content: &[u8], undefined: bool) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0xFFFEu16.to_le_bytes());
        out.extend_from_slice(&0xE000u16.to_le_bytes());
        if undefined {
            out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            out.extend_from_slice(content);
            out.extend_from_slice(&0xFFFEu16.to_le_bytes());
            out.extend_from_slice(&0xE00Du16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
        } else {
            out.extend_from_slice(&(content.len() as u32).to_le_bytes());
            out.extend_from_slice(content);
        }
        out
    }

    fn sequence_delimiter() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0xFFFEu16.to_le_bytes());
        out.extend_from_slice(&0xE0DDu16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    fn parse(bytes: &[u8], syntax: &Arc<TransferSyntax>) -> Dataset {
        parse_dataset_bytes(bytes, syntax).unwrap()
    }

    #[test]
    fn explicit_le_basic_elements() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&el_header(0x0010, 0x0010, "PN", b"Doe^John"));
        bytes.extend_from_slice(&el_header(0x0010, 0x0020, "LO", b"PID-1"));
        bytes.extend_from_slice(&el_header(0x0028, 0x0010, "IS", b"512"));
        let ds = parse(&bytes, &le());
        assert_eq!(ds.get_str(Tag::PATIENT_NAME).unwrap(), "Doe^John");
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
        let rows: u16 = ds.get(Tag::ROWS).unwrap();
        assert_eq!(rows, 512);
    }

    #[test]
    fn implicit_le_with_dictionary_vr() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0010u16.to_le_bytes());
        bytes.extend_from_slice(&0x0020u16.to_le_bytes());
        bytes.extend_from_slice(&5u32.to_le_bytes());
        bytes.extend_from_slice(b"PID-1");
        // group length: implicit resolves to UL (T9 shape)
        bytes.extend_from_slice(&0x0008u16.to_le_bytes());
        bytes.extend_from_slice(&0x0000u16.to_le_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&12u32.to_le_bytes());
        let ds = parse(&bytes, &implicit());
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
        let gl: u32 = ds.get(Tag::new(0x0008, 0x0000)).unwrap();
        assert_eq!(gl, 12, "group length reads as UL (T9 shape)");
    }

    #[test]
    fn t1_two_space_vr_uses_dictionary() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0010u16.to_le_bytes());
        bytes.extend_from_slice(&0x0020u16.to_le_bytes());
        bytes.extend_from_slice(b"  "); // not a valid VR
        bytes.extend_from_slice(&5u16.to_le_bytes());
        bytes.extend_from_slice(b"PID-1");
        let ds = parse(&bytes, &le());
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1", "T1: dictionary VR");
    }

    #[test]
    fn t2_garbage_vr_falls_back_to_implicit() {
        // Real T2 shape: an implicit-VR element inside a dataset the parser
        // believes is explicit. The 4-byte length's leading bytes are not a
        // VR, so the parser rewinds and reads this element implicitly.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0010u16.to_le_bytes());
        bytes.extend_from_slice(&0x0020u16.to_le_bytes());
        bytes.extend_from_slice(&5u32.to_le_bytes()); // 05 00 00 00
        bytes.extend_from_slice(b"PID-1");
        let ds = parse(&bytes, &le());
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1", "T2: implicit fallback");
    }

    #[test]
    fn t3_un_undefined_length_parsed_as_sq() {
        // (0008,1140) written with VR UN + undefined length containing one item
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0008u16.to_le_bytes());
        bytes.extend_from_slice(&0x1140u16.to_le_bytes());
        bytes.extend_from_slice(b"UN");
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&item(&el_header(0x0008, 0x1150, "UI", b"1.2.3"), false));
        bytes.extend_from_slice(&sequence_delimiter());

        let ds = parse(&bytes, &le());
        let seq = ds.get_sequence(Tag::REFERENCED_IMAGE_SEQUENCE).unwrap();
        assert_eq!(seq.len(), 1, "T3: UN + undefined length reads as SQ");
        assert_eq!(seq.items()[0].get_str(Tag::REFERENCED_SOP_CLASS_UID).unwrap(), "1.2.3");
    }

    #[test]
    fn defined_length_sequence_and_nesting() {
        let mut sq = Vec::new();
        sq.extend_from_slice(&item(&el_header(0x0008, 0x1150, "UI", b"1.2.3"), false));
        sq.extend_from_slice(&item(&el_header(0x0008, 0x1155, "UI", b"4.5.6"), false));
        sq.extend_from_slice(&sequence_delimiter());

        let mut bytes = el_long(0x0008, 0x1140, "SQ", &sq);
        // nested SQ: item containing an inner sequence
        let inner_sq = el_long(0x0008, 0x1140, "SQ", &[item(&el_header(0x0008, 0x1150, "UI", b"7.8.9"), false)].concat());
        bytes.extend_from_slice(&item(&inner_sq, true));

        let ds = parse(&bytes, &le());
        let seq = ds.get_sequence(Tag::REFERENCED_IMAGE_SEQUENCE).unwrap();
        assert_eq!(seq.len(), 2);
        assert_eq!(seq.items()[0].get_str(Tag::REFERENCED_SOP_CLASS_UID).unwrap(), "1.2.3");
        assert_eq!(seq.items()[1].get_str(Tag::REFERENCED_SOP_INSTANCE_UID).unwrap(), "4.5.6");
    }

    #[test]
    fn nested_sequences_build_inner_datasets() {
        let inner = el_long(0x0008, 0x1140, "SQ", &[item(&el_header(0x0008, 0x1150, "UI", b"1.2.3"), false)].concat());
        let bytes = item(&inner, false);
        let mut outer = el_long(0x0008, 0x1115, "SQ", &bytes);
        outer.extend_from_slice(&sequence_delimiter());
        let ds = parse(&outer, &le());
        let outer_seq = ds.get_sequence(Tag::REFERENCED_SERIES_SEQUENCE).unwrap();
        assert_eq!(outer_seq.len(), 1);
        let inner_seq = outer_seq.items()[0].get_sequence(Tag::REFERENCED_IMAGE_SEQUENCE).unwrap();
        assert_eq!(inner_seq.len(), 1);
        assert_eq!(inner_seq.items()[0].get_str(Tag::REFERENCED_SOP_CLASS_UID).unwrap(), "1.2.3");
    }

    #[test]
    fn undefined_length_item_with_delimiter() {
        let mut bytes = el_long(0x0008, 0x1140, "SQ", &[]);
        bytes.truncate(bytes.len() - 4); // drop the defined length
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&item(&el_header(0x0008, 0x1150, "UI", b"1.2.3"), true));
        bytes.extend_from_slice(&sequence_delimiter());
        let ds = parse(&bytes, &le());
        let seq = ds.get_sequence(Tag::REFERENCED_IMAGE_SEQUENCE).unwrap();
        assert_eq!(seq.len(), 1);
        assert_eq!(seq.items()[0].get_str(Tag::REFERENCED_SOP_CLASS_UID).unwrap(), "1.2.3");
    }

    #[test]
    fn fragments_with_offset_table() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x7FE0u16.to_le_bytes());
        bytes.extend_from_slice(&0x0010u16.to_le_bytes());
        bytes.extend_from_slice(b"OB");
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // offset table: 1 entry = 0
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE000u16.to_le_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        // fragment 1
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE000u16.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0xAA, 0xBB]);
        bytes.extend_from_slice(&sequence_delimiter());

        let ds = parse(&bytes, &le());
        let frags = match ds.item(&Tag::PIXEL_DATA) {
            Some(DicomItem::Fragments(f)) => f,
            other => panic!("expected fragments, got {other:?}"),
        };
        assert_eq!(frags.offset_table(), &[0]);
        assert_eq!(frags.fragments().len(), 1);
        assert_eq!(&*frags.fragments()[0].data(), &[0xAA, 0xBB]);
    }

    #[test]
    fn t5_orphan_item_treated_as_undefined_sq() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE000u16.to_le_bytes());
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&el_header(0x0008, 0x1150, "UI", b"1.2.3"));
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE00Du16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&sequence_delimiter());

        let ds = parse(&bytes, &le());
        let seq = ds.get_sequence(Tag::ITEM).expect("T5: orphan item becomes SQ");
        assert_eq!(seq.tag(), &Tag::new(0xFFFE, 0xE000), "PS3.5 §7.5 item identity");
        assert_eq!(seq.len(), 1);
        assert_eq!(seq.items()[0].get_str(Tag::REFERENCED_SOP_CLASS_UID).unwrap(), "1.2.3");
    }

    #[test]
    fn t6_missing_fragment_delimiter_tolerated() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x7FE0u16.to_le_bytes());
        bytes.extend_from_slice(&0x0010u16.to_le_bytes());
        bytes.extend_from_slice(b"OB");
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE000u16.to_le_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE000u16.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2]);
        // NO 0xE0DD delimiter — EOF

        let ds = parse(&bytes, &le());
        match ds.item(&Tag::PIXEL_DATA) {
            Some(DicomItem::Fragments(f)) => {
                assert_eq!(f.fragments().len(), 1, "T6: offset table is not a fragment");
            }
            other => panic!("expected fragments, got {other:?}"),
        }
    }

    #[test]
    fn t7_sequence_ends_where_item_expected() {
        // k-pacs style: undefined-length SQ whose last item is terminated by
        // the *sequence* delimiter instead of an item delimiter
        let mut bytes = el_long(0x0008, 0x1140, "SQ", &[]);
        bytes.truncate(bytes.len() - 4);
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE000u16.to_le_bytes());
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&el_header(0x0008, 0x1150, "UI", b"1.2.3"));
        // straight to the SQ delimiter, no FFFE,E00D
        bytes.extend_from_slice(&sequence_delimiter());
        // a following element must still be reachable
        bytes.extend_from_slice(&el_header(0x0010, 0x0020, "LO", b"PID-1"));

        let ds = parse(&bytes, &le());
        let seq = ds.get_sequence(Tag::REFERENCED_IMAGE_SEQUENCE).unwrap();
        assert_eq!(seq.len(), 1, "T7: unterminated item is accepted");
        assert_eq!(seq.items()[0].get_str(Tag::REFERENCED_SOP_CLASS_UID).unwrap(), "1.2.3");
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
    }

    #[test]
    fn t9_group_length_with_un_vr_reads_as_ul() {
        // Explicit dataset where a group length carries VR UN (some old
        // writers did this); it must be read as UL so lengths stay numeric
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0008u16.to_le_bytes());
        bytes.extend_from_slice(&0x0000u16.to_le_bytes());
        bytes.extend_from_slice(b"UN");
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&12u32.to_le_bytes());
        let ds = parse(&bytes, &le());
        let gl: u32 = ds.get(Tag::new(0x0008, 0x0000)).unwrap();
        assert_eq!(gl, 12, "T9: UN group length reads as UL");
    }

    #[test]
    fn t4_private_sq_with_explicit_inner_in_implicit_dataset() {
        // Implicit dataset holding a private undefined-length SQ whose inner
        // elements were written explicitly — the T4 probe must switch mode
        // for this subtree only, leaving the outer dataset implicit.
        let inner = el_header(0x0029, 0x1010, "LO", b"widget");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0029u16.to_le_bytes());
        bytes.extend_from_slice(&0x1001u16.to_le_bytes());
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&item(&inner, true));
        bytes.extend_from_slice(&sequence_delimiter());
        // a following implicit element proves the outer mode was restored
        bytes.extend_from_slice(&0x0010u16.to_le_bytes());
        bytes.extend_from_slice(&0x0020u16.to_le_bytes());
        bytes.extend_from_slice(&5u32.to_le_bytes());
        bytes.extend_from_slice(b"PID-1");

        let ds = parse(&bytes, &implicit());
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1", "T4: outer mode restored");
        let seq = ds.get_sequence(Tag::new(0x0029, 0x1001)).expect("T4: private SQ parsed");
        assert_eq!(seq.len(), 1);
        assert_eq!(seq.items()[0].get_str(Tag::new(0x0029, 0x1010)).unwrap(), "widget");
    }

    #[test]
    fn t10_deflated_dataset_is_inflated() {
        use std::io::Write as _;
        let mut plain = Vec::new();
        plain.extend_from_slice(&el_header(0x0010, 0x0020, "LO", b"PID-1"));
        // PS3.5 A.5: the body is raw DEFLATE (RFC 1951), not zlib-wrapped
        let mut deflated = Vec::new();
        let mut encoder =
            flate2::write::DeflateEncoder::new(&mut deflated, flate2::Compression::default());
        encoder.write_all(&plain).unwrap();
        encoder.finish().unwrap();

        let syntax = syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let ds = parse(&deflated, &syntax);
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1", "T10: transparent inflate");
    }

    #[test]
    fn t10_zlib_wrapped_body_still_inflates() {
        // some non-conformant writers wrap the deflate body in zlib; the reader
        // falls back to the zlib decoder for those
        use std::io::Write as _;
        let mut plain = Vec::new();
        plain.extend_from_slice(&el_header(0x0010, 0x0020, "LO", b"PID-1"));
        let mut deflated = Vec::new();
        let mut encoder =
            flate2::write::ZlibEncoder::new(&mut deflated, flate2::Compression::default());
        encoder.write_all(&plain).unwrap();
        encoder.finish().unwrap();

        let syntax = syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let ds = parse(&deflated, &syntax);
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1", "zlib fallback inflate");
    }

    #[test]
    fn t8_trailing_garbage_ignored() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&el_header(0x0010, 0x0020, "LO", b"PID-1"));
        bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let ds = parse(&bytes, &le());
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
        assert_eq!(ds.len(), 1, "T8: trailing garbage ignored");
    }

    #[test]
    fn private_creator_tracking_binds_tags() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&el_header(0x0029, 0x0010, "LO", b"ACME 1.2"));
        bytes.extend_from_slice(&el_header(0x0029, 0x1001, "LO", b"widget"));
        let ds = parse(&bytes, &le());
        let tag = ds
            .items()
            .find_map(|item| {
                let t = item.tag();
                if t.group() == 0x0029 && t.element() == 0x1001 {
                    Some(t.clone())
                } else {
                    None
                }
            })
            .expect("private tag present");
        assert_eq!(tag.private_creator().map(|c| c.as_str()), Some("ACME 1.2"), "RD-05: creator bound");
        assert_eq!(ds.get_str(tag).unwrap(), "widget");
    }

    #[test]
    fn stop_predicate_rewinds() {
        let first = el_header(0x0008, 0x0050, "SH", b"ACC1");
        let second = el_header(0x0010, 0x0020, "LO", b"PID-1");
        let third = el_header(0x0010, 0x0010, "PN", b"Doe^John");
        let bytes: Vec<u8> = [first, second, third.clone()].concat();

        let mut source = crate::io::byte_source::ByteSourceReader::new(
            std::io::Cursor::new(bytes.clone()),
            crate::model::endian::Endian::Little,
        );
        let reader = crate::io::reader::DicomReader::default();
        let mut builder = DatasetBuilder::with_transfer_syntax(le());
        let stop = |state: &crate::io::reader::ParseState| state.tag == (0x0010, 0x0010);
        let outcome = reader.read(&mut source, &le(), &mut builder, Some(&stop)).unwrap();
        assert_eq!(outcome, crate::io::reader::ReadOutcome::Stopped);

        // everything before the stop tag is built; the stop tag is not
        let ds = builder.into_dataset();
        assert_eq!(ds.len(), 2);
        assert_eq!(ds.get_str(Tag::ACCESSION_NUMBER).unwrap(), "ACC1");
        assert_eq!(ds.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
        assert!(!ds.contains(&Tag::PATIENT_NAME));
        // the source was rewound to the element start
        assert_eq!(source.position().unwrap(), (bytes.len() - third.len()) as u64);

        // resuming from there parses the remaining element
        let mut builder2 = DatasetBuilder::with_transfer_syntax(le());
        let outcome = reader.read(&mut source, &le(), &mut builder2, None).unwrap();
        assert_eq!(outcome, crate::io::reader::ReadOutcome::Success);
        let resumed = builder2.into_dataset();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed.get_str(Tag::PATIENT_NAME).unwrap(), "Doe^John");
    }

    #[test]
    fn depth_guard_rejects_runaway_nesting() {
        let max = crate::io::reader::DicomReader::default().max_depth;
        let mut bytes = Vec::new();
        for _ in 0..(max + 10) {
            bytes.extend_from_slice(&0x0008u16.to_le_bytes());
            bytes.extend_from_slice(&0x1140u16.to_le_bytes());
            bytes.extend_from_slice(b"UN");
            bytes.extend_from_slice(&[0, 0]);
            bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
            bytes.extend_from_slice(&0xE000u16.to_le_bytes());
            bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        let result = parse_dataset_bytes(&bytes, &le());
        assert!(matches!(result, Err(Error::Reader(ReaderError::TooDeep { .. }))), "depth cap fires");
    }

    #[test]
    fn truncated_value_reports_real_offset() {
        let mut bytes = el_header(0x0010, 0x0020, "LO", b"PID-1");
        bytes.truncate(bytes.len() - 2);
        match parse_dataset_bytes(&bytes, &le()) {
            Err(Error::Reader(ReaderError::Eof { position, needed })) => {
                assert_eq!(position, 8, "value starts right after the header");
                assert_eq!(needed, 5, "the whole declared length is missing");
            }
            other => panic!("expected Eof, got {other:?}"),
        }
    }

    #[test]
    fn big_endian_elements() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0018u16.to_be_bytes());
        bytes.extend_from_slice(&0x0060u16.to_be_bytes());
        bytes.extend_from_slice(b"IS");
        bytes.extend_from_slice(&4u16.to_be_bytes());
        bytes.extend_from_slice(b"0072");
        let ds = parse(&bytes, &syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone());
        let hr: i32 = ds.get(Tag::KVP).unwrap();
        assert_eq!(hr, 72);
    }

    #[test]
    fn big_endian_binary_value_is_swapped() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0028u16.to_be_bytes());
        bytes.extend_from_slice(&0x0010u16.to_be_bytes()); // Rows, US
        bytes.extend_from_slice(b"US");
        bytes.extend_from_slice(&2u16.to_be_bytes());
        bytes.extend_from_slice(&512u16.to_be_bytes());
        let ds = parse(&bytes, &syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone());
        let rows: u16 = ds.get(Tag::ROWS).unwrap();
        assert_eq!(rows, 512, "US value is byte-swapped on read");
    }

    #[test]
    fn deeply_nested_sequences_hit_the_depth_cap_not_a_stack_overflow() {
        // 300 nested undefined-length SQ/items: the reader must stop at max_depth
        // (256) with TooDeep rather than recursing to a stack overflow.
        let levels = 300u32;
        let mut bytes = Vec::new();
        for _ in 0..levels {
            // SQ (0008,1111), undefined length, then an undefined-length item
            bytes.extend_from_slice(&0x0008u16.to_le_bytes());
            bytes.extend_from_slice(&0x1111u16.to_le_bytes());
            bytes.extend_from_slice(b"SQ");
            bytes.extend_from_slice(&[0, 0]);
            bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
            bytes.extend_from_slice(&0xE000u16.to_le_bytes());
            bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        bytes.extend_from_slice(&el_header(0x0010, 0x0010, "PN", b"X "));
        for _ in 0..levels {
            bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
            bytes.extend_from_slice(&0xE00Du16.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&sequence_delimiter());
        }
        match parse_dataset_bytes(&bytes, &le()) {
            Err(Error::Reader(ReaderError::TooDeep { .. })) => {}
            other => panic!("expected TooDeep, got {other:?}"),
        }
    }

    #[test]
    fn declared_length_beyond_the_file_errors_without_allocating() {
        // OB element claiming ~2 GB with no value bytes: must fail cleanly (Eof),
        // never attempt a 2 GB allocation.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x7FE0u16.to_le_bytes());
        bytes.extend_from_slice(&0x0010u16.to_le_bytes());
        bytes.extend_from_slice(b"OB");
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        assert!(parse_dataset_bytes(&bytes, &le()).is_err(), "over-declared length must error");
    }

    #[test]
    fn fragment_sequence_delimiter_length_is_consumed() {
        // 52-byte encapsulated value followed by a real element: the
        // SequenceDelimitationItem's 4-byte length must be consumed, otherwise
        // the following element is read from the wrong offset.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x7FE0u16.to_le_bytes());
        bytes.extend_from_slice(&0x0010u16.to_le_bytes());
        bytes.extend_from_slice(b"OB");
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE000u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // empty basic offset table
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE000u16.to_le_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE0DDu16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&el_header(0x0008, 0x0050, "SH", b"TAIL"));

        let ds = parse(&bytes, &le());
        assert_eq!(ds.get_str(Tag::ACCESSION_NUMBER).unwrap(), "TAIL", "stream stayed in sync");
        let fragments = match ds.item(&Tag::PIXEL_DATA) {
            Some(crate::model::item::DicomItem::Fragments(f)) => f.fragments().to_vec(),
            other => panic!("expected fragments, got {other:?}"),
        };
        assert_eq!(fragments.len(), 1, "one data fragment besides the offset table");
        assert_eq!(fragments[0].data().as_ref(), &[1, 2, 3, 4]);
    }

    #[test]
    fn cp246_big_endian_un_sequence_reads_its_contents_as_little_endian() {
        // PS3.5 CP-246: a UN element with undefined length in a Big Endian
        // dataset contains Implicit VR Little Endian item data.
        let mut bytes = Vec::new();
        // (0008,2112) UN, undefined length, Big Endian header
        bytes.extend_from_slice(&0x0008u16.to_be_bytes());
        bytes.extend_from_slice(&0x2112u16.to_be_bytes());
        bytes.extend_from_slice(b"UN");
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        // item header and body in little endian
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE000u16.to_le_bytes());
        bytes.extend_from_slice(&13u32.to_le_bytes());
        bytes.extend_from_slice(&0x0010u16.to_le_bytes());
        bytes.extend_from_slice(&0x0020u16.to_le_bytes());
        bytes.extend_from_slice(&5u32.to_le_bytes());
        bytes.extend_from_slice(b"PID-1");
        // sequence delimiter in little endian
        bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
        bytes.extend_from_slice(&0xE0DDu16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        // a following Big Endian element proves the byte order was restored
        bytes.extend_from_slice(&0x0008u16.to_be_bytes());
        bytes.extend_from_slice(&0x0050u16.to_be_bytes());
        bytes.extend_from_slice(b"SH");
        bytes.extend_from_slice(&4u16.to_be_bytes());
        bytes.extend_from_slice(b"TAIL");

        let ds = parse(&bytes, &syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone());
        let seq = ds.get_sequence(Tag::SOURCE_IMAGE_SEQUENCE).expect("CP-246 sequence");
        assert_eq!(seq.len(), 1);
        assert_eq!(seq.items()[0].get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
        assert_eq!(ds.get_str(Tag::ACCESSION_NUMBER).unwrap(), "TAIL", "byte order restored");
    }

    #[test]
    fn implicit_private_tag_uses_the_private_dictionary_vr() {
        // (0009,1001) with creator GEMS_IDEN_01 is LO in the private dictionary;
        // implicit VR must resolve it instead of falling back to UN.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0009u16.to_le_bytes());
        bytes.extend_from_slice(&0x0010u16.to_le_bytes());
        bytes.extend_from_slice(&12u32.to_le_bytes());
        bytes.extend_from_slice(b"GEMS_IDEN_01");
        bytes.extend_from_slice(&0x0009u16.to_le_bytes());
        bytes.extend_from_slice(&0x1001u16.to_le_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(b"WIDG");

        let ds = parse(&bytes, &implicit());
        let tag = ds
            .items()
            .find_map(|item| {
                let t = item.tag();
                (t.group() == 0x0009 && t.element() == 0x1001).then(|| t.clone())
            })
            .expect("private tag present");
        assert_eq!(tag.private_creator().map(|c| c.as_str()), Some("GEMS_IDEN_01"));
        let element = ds.get_element(&tag).unwrap();
        assert_eq!(element.vr(), crate::model::vr::Vr::Lo, "private dictionary VR wins over UN");
        assert_eq!(ds.get_str(&tag).unwrap(), "WIDG");
    }
}