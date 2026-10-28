//! Dataset writer and length calculator (WR-01..03; docs/01-architecture.md §7).
//!
//! Writing and length computation share one set of header/length helpers so
//! an explicit-length sequence can never disagree with its own byte count.

use crate::encoding::charset::{charset_encoding, encode_multi_charset};
use crate::error::{DataError, Error};
use crate::io::byte_source::ByteTargetWriter;
use crate::model::buffer::ByteBuffer;
use crate::model::dataset::Dataset;
use crate::model::element::DicomElement;
use crate::model::endian::Endian;
use crate::model::item::{DicomFragmentSequence, DicomItem, DicomSequence};
use crate::model::tag::Tag;
use crate::model::transfer_syntax::TransferSyntax;
use crate::model::vr::Vr;
use std::io::Write;

/// Writing switches (WR-03, docs/01-architecture.md §7).
#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    /// Emit defined lengths for sequences instead of delimiters.
    pub explicit_length_sequences: bool,
    /// Emit defined lengths for sequence items instead of delimiters.
    pub explicit_length_sequence_items: bool,
    /// Recalculate `(GGGG,0000)` group lengths; otherwise they are removed.
    pub keep_group_lengths: bool,
}

/// A resolved SpecificCharacterSet for one dataset level (ENC-03).
struct Charsets {
    first: &'static encoding_rs::Encoding,
    alternates: Vec<&'static encoding_rs::Encoding>,
}

impl Charsets {
    /// Reads `(0008,0005)` from `ds`; falls back to the enclosing level.
    fn resolve(ds: &Dataset, inherited: Option<&Charsets>) -> Option<Charsets> {
        let inherit = || inherited.map(|c| Charsets { first: c.first, alternates: c.alternates.clone() });
        let Some(DicomItem::Element(element)) = ds.item(&Tag::SPECIFIC_CHARACTER_SET) else {
            return inherit();
        };
        let text = element.string_value();
        let mut terms = text.split('\\').map(str::trim).filter(|t| !t.is_empty());
        let Some(first) = terms.next() else {
            return inherit();
        };
        Some(Charsets {
            first: charset_encoding(first),
            alternates: terms.map(charset_encoding).collect(),
        })
    }
}

/// DICOM dataset writer (WR-01).
#[derive(Debug, Clone, Default)]
pub struct DicomWriter {
    pub options: WriteOptions,
}

impl DicomWriter {
    pub fn new(options: WriteOptions) -> Self {
        DicomWriter { options }
    }

    /// Writes `ds` encoded with `syntax`; returns the byte count.
    pub fn write_dataset<W: Write>(
        &self,
        ds: &Dataset,
        syntax: &TransferSyntax,
        out: &mut W,
    ) -> Result<u64, Error> {
        let mut target = ByteTargetWriter::new(out, syntax.endian());
        let mut written = 0u64;
        self.write_dataset_into(ds, syntax, None, &mut target, &mut written)?;
        Ok(written)
    }

    /// Encoded size of `ds` under `syntax` (WR-02).
    pub fn dataset_length(&self, ds: &Dataset, syntax: &TransferSyntax) -> Result<u64, Error> {
        self.dataset_len(ds, syntax, None)
    }

    // -- write path ---------------------------------------------------------

    fn write_dataset_into<W: Write>(
        &self,
        ds: &Dataset,
        syntax: &TransferSyntax,
        inherited: Option<&Charsets>,
        out: &mut ByteTargetWriter<W>,
        written: &mut u64,
    ) -> Result<(), Error> {
        let charset = Charsets::resolve(ds, inherited);
        let charset = charset.as_ref();
        let mut group: Option<u16> = None;
        for item in ds.items() {
            let g = item.tag().group();
            let is_group_length = item.tag().element() == 0x0000;
            if self.options.keep_group_lengths && group != Some(g) {
                group = Some(g);
                let len = self.group_length(ds, g, syntax, charset)?;
                // UL is binary: its bytes follow the target byte order
                let len_bytes = match syntax.endian() {
                    Endian::Little => len.to_le_bytes(),
                    Endian::Big => len.to_be_bytes(),
                };
                *written += self.write_leaf(Tag::new(g, 0x0000), Vr::Ul, ValueRef::Bytes(&len_bytes), syntax, out)?;
            }
            if is_group_length {
                // removed unless recalculated above (PS3.5 §7.2)
                continue;
            }
            *written += self.write_item(item, syntax, charset, out)?;
        }
        Ok(())
    }

    fn write_item<W: Write>(
        &self,
        item: &DicomItem,
        syntax: &TransferSyntax,
        charset: Option<&Charsets>,
        out: &mut ByteTargetWriter<W>,
    ) -> Result<u64, Error> {
        match item {
            DicomItem::Element(e) => {
                let bytes = encode_value(e, charset);
                match bytes {
                    Some(value) => self.write_leaf(e.tag().clone(), e.vr(), ValueRef::Bytes(&value), syntax, out),
                    // binary VR: stream the buffer, swapping when the target
                    // byte order differs from the in-memory (native) order
                    None => self.write_binary(e, syntax, out),
                }
            }
            DicomItem::Sequence(seq) => self.write_sequence(seq, syntax, charset, out),
            DicomItem::Fragments(f) => self.write_fragments(f, syntax, out),
        }
    }

    fn write_sequence<W: Write>(
        &self,
        seq: &DicomSequence,
        syntax: &TransferSyntax,
        charset: Option<&Charsets>,
        out: &mut ByteTargetWriter<W>,
    ) -> Result<u64, Error> {
        let body = self.sequence_body_len(seq, syntax, charset)?;
        // private SQs are always explicit-length (fo-dicom rule, §5.2)
        let defined = self.options.explicit_length_sequences || seq.tag().is_private();
        let len = if defined { Some(body) } else { None };
        let mut written = self.write_header(seq.tag().clone(), Vr::Sq, len, syntax, out)?;
        for item_ds in seq.items() {
            written += self.write_item_header(item_ds, syntax, charset, out)?;
        }
        if !defined {
            written += write_delimiter(0xE0DD, out)?;
        }
        Ok(written)
    }

    /// Writes one sequence item: header, content and (when undefined) the
    /// item delimiter.
    fn write_item_header<W: Write>(
        &self,
        item: &Dataset,
        syntax: &TransferSyntax,
        charset: Option<&Charsets>,
        out: &mut ByteTargetWriter<W>,
    ) -> Result<u64, Error> {
        let body = self.dataset_len(item, syntax, charset)?;
        let defined = self.options.explicit_length_sequence_items;
        let mut written = write_item_tag(out, body, defined)?;
        let mut inner = 0u64;
        self.write_dataset_into(item, syntax, charset, out, &mut inner)?;
        written += inner;
        if !defined {
            written += write_delimiter(0xE00D, out)?;
        }
        Ok(written)
    }

    fn write_fragments<W: Write>(
        &self,
        f: &DicomFragmentSequence,
        syntax: &TransferSyntax,
        out: &mut ByteTargetWriter<W>,
    ) -> Result<u64, Error> {
        // encapsulated data always uses undefined length + delimiters
        let mut written = self.write_header(f.tag().clone(), f.vr(), None, syntax, out)?;
        // fragment 0: the basic offset table
        let table = f.offset_table();
        let mut table_bytes = Vec::with_capacity(table.len() * 4);
        for offset in table {
            table_bytes.extend_from_slice(&match syntax.endian() {
                Endian::Little => offset.to_le_bytes(),
                Endian::Big => offset.to_be_bytes(),
            });
        }
        written += write_item_tag(out, table_bytes.len() as u64, true)?;
        out.write_bytes(&table_bytes).map_err(io_err)?;
        written += table_bytes.len() as u64;
        for fragment in f.fragments() {
            // item lengths must be even; the pad byte is part of the value
            let pad = fragment.size() % 2;
            written += write_item_tag(out, fragment.size() + pad, true)?;
            written += write_buffer(out, fragment.as_ref(), f.vr().byte_swap_size(), syntax.endian())?;
            if pad == 1 {
                out.write_u8(0).map_err(io_err)?;
                written += 1;
            }
        }
        written += write_delimiter(0xE0DD, out)?;
        Ok(written)
    }

    /// A leaf element whose value is fully materialized.
    fn write_leaf<W: Write>(
        &self,
        tag: Tag,
        vr: Vr,
        value: ValueRef<'_>,
        syntax: &TransferSyntax,
        out: &mut ByteTargetWriter<W>,
    ) -> Result<u64, Error> {
        let value_len = value.len();
        let mut written = self.write_header(tag, vr, Some(value_len), syntax, out)?;
        match value {
            ValueRef::Bytes(b) => out.write_bytes(b).map_err(io_err)?,
        }
        written += value_len;
        Ok(written)
    }

    /// A binary element streamed from its buffer.
    fn write_binary<W: Write>(
        &self,
        e: &DicomElement,
        syntax: &TransferSyntax,
        out: &mut ByteTargetWriter<W>,
    ) -> Result<u64, Error> {
        let Some(buffer) = e.value().as_buffer() else {
            return Err(Error::Data(DataError::Other {
                message: format!("{}: expected a byte value", e.tag()),
            }));
        };
        let size = buffer.size();
        let pad = size % 2;
        let mut written = self.write_header(e.tag().clone(), e.vr(), Some(size + pad), syntax, out)?;
        written += write_buffer(out, buffer.as_ref(), e.vr().byte_swap_size(), syntax.endian())?;
        if pad == 1 {
            out.write_u8(e.vr().padding()).map_err(io_err)?;
            written += 1;
        }
        Ok(written)
    }

    /// Tag + VR + length header (WR-01, incl. the CP-1066 demotion guard).
    /// `value_len = None` writes the undefined length `0xFFFFFFFF`.
    fn write_header<W: Write>(
        &self,
        tag: Tag,
        vr: Vr,
        value_len: Option<u64>,
        syntax: &TransferSyntax,
        out: &mut ByteTargetWriter<W>,
    ) -> Result<u64, Error> {
        let len = value_len.unwrap_or(0xFFFF_FFFF);
        out.write_u16(tag.group()).map_err(io_err)?;
        out.write_u16(tag.element()).map_err(io_err)?;
        let mut written = 4u64;
        let (vr, long_form) = effective_vr(vr, len);
        if syntax.is_explicit_vr() {
            out.write_bytes(vr.code().as_bytes()).map_err(io_err)?;
            written += 2;
            if long_form {
                out.write_u16(0).map_err(io_err)?;
                out.write_u32(len as u32).map_err(io_err)?;
                written += 6;
            } else {
                out.write_u16(len as u16).map_err(io_err)?;
                written += 2;
            }
        } else {
            out.write_u32(len as u32).map_err(io_err)?;
            written += 4;
        }
        Ok(written)
    }

    // -- length path (WR-02) ------------------------------------------------

    fn dataset_len(
        &self,
        ds: &Dataset,
        syntax: &TransferSyntax,
        inherited: Option<&Charsets>,
    ) -> Result<u64, Error> {
        let charset = Charsets::resolve(ds, inherited);
        let charset = charset.as_ref();
        let mut total = 0u64;
        let mut group: Option<u16> = None;
        for item in ds.items() {
            let g = item.tag().group();
            if self.options.keep_group_lengths && group != Some(g) {
                group = Some(g);
                // a UL value is always 4 bytes, so the size does not depend
                // on the computed length
                total += leaf_len(Tag::new(g, 0x0000), Vr::Ul, 4, syntax);
            }
            if item.tag().element() == 0x0000 {
                continue;
            }
            total += self.item_len(item, syntax, charset)?;
        }
        Ok(total)
    }

    fn item_len(
        &self,
        item: &DicomItem,
        syntax: &TransferSyntax,
        charset: Option<&Charsets>,
    ) -> Result<u64, Error> {
        Ok(match item {
            DicomItem::Element(e) => {
                let value_len = match encode_value(e, charset) {
                    Some(bytes) => bytes.len() as u64,
                    None => {
                        let size = e.value().as_buffer().map(|b| b.size()).unwrap_or(0);
                        size + size % 2
                    }
                };
                leaf_len(e.tag().clone(), e.vr(), value_len, syntax)
            }
            DicomItem::Sequence(seq) => {
                let body = self.sequence_body_len(seq, syntax, charset)?;
                let defined = self.options.explicit_length_sequences || seq.tag().is_private();
                header_len(seq.tag().clone(), Vr::Sq, body, syntax)
                    + body
                    + if defined { 0 } else { 8 }
            }
            DicomItem::Fragments(f) => {
                let mut total = header_len(f.tag().clone(), f.vr(), 0, syntax);
                let table = f.offset_table().len() as u64 * 4;
                total += 8 + table;
                for fragment in f.fragments() {
                    total += 8 + fragment.size() + fragment.size() % 2;
                }
                total + 8
            }
        })
    }

    fn sequence_body_len(
        &self,
        seq: &DicomSequence,
        syntax: &TransferSyntax,
        charset: Option<&Charsets>,
    ) -> Result<u64, Error> {
        let mut total = 0u64;
        for item in seq.items() {
            let body = self.dataset_len(item, syntax, charset)?;
            total += 8 + body;
            if !self.options.explicit_length_sequence_items {
                total += 8;
            }
        }
        Ok(total)
    }

    /// Bytes covered by one group, excluding its group length element.
    fn group_length(
        &self,
        ds: &Dataset,
        group: u16,
        syntax: &TransferSyntax,
        charset: Option<&Charsets>,
    ) -> Result<u32, Error> {
        let mut total = 0u64;
        for item in ds.items() {
            if item.tag().group() != group || item.tag().element() == 0x0000 {
                continue;
            }
            total += self.item_len(item, syntax, charset)?;
        }
        Ok(total as u32)
    }
}

/// A materialized value being written.
enum ValueRef<'a> {
    Bytes(&'a [u8]),
}

impl ValueRef<'_> {
    fn len(&self) -> u64 {
        match self {
            ValueRef::Bytes(b) => b.len() as u64,
        }
    }
}

/// Encodes a string value with the level's charset; `None` for binary VRs,
/// which are streamed straight from their buffer.
fn encode_value(e: &DicomElement, charset: Option<&Charsets>) -> Option<Vec<u8>> {
    if !e.vr().is_string() {
        return None;
    }
    let text = e.string_value();
    let mut bytes = match charset {
        Some(c) if e.vr().is_character_encoded() => {
            encode_multi_charset(text, c.first, &c.alternates, e.vr() == Vr::Pn)
        }
        _ => text.as_bytes().to_vec(),
    };
    if bytes.len() % 2 == 1 {
        bytes.push(e.vr().padding());
    }
    Some(bytes)
}

/// CP-1066 guard: a value too long for a 16-bit length field is demoted to
/// UN and written with the 32-bit form.
fn effective_vr(vr: Vr, value_len: u64) -> (Vr, bool) {
    let long_form = !vr.is_16bit_length() || value_len > 0xFFFE;
    if vr.is_16bit_length() && value_len > 0xFFFE {
        (Vr::Un, true)
    } else {
        (vr, long_form)
    }
}

fn leaf_len(tag: Tag, vr: Vr, value_len: u64, syntax: &TransferSyntax) -> u64 {
    header_len(tag, vr, value_len, syntax) + value_len
}

fn header_len(_tag: Tag, vr: Vr, value_len: u64, syntax: &TransferSyntax) -> u64 {
    let (_, long_form) = effective_vr(vr, value_len);
    if syntax.is_explicit_vr() {
        if long_form {
            12
        } else {
            8
        }
    } else {
        8
    }
}

/// `FFFE,XXXX` item header.
fn write_item_tag<W: Write>(out: &mut ByteTargetWriter<W>, len: u64, defined: bool) -> Result<u64, Error> {
    out.write_u16(0xFFFE).map_err(io_err)?;
    out.write_u16(0xE000).map_err(io_err)?;
    let value = if defined { len as u32 } else { 0xFFFF_FFFF };
    out.write_u32(value).map_err(io_err)?;
    Ok(8)
}

/// Item/sequence delimiter (`FFFE,E00D` / `FFFE,E0DD`) with a zero length.
fn write_delimiter<W: Write>(element: u16, out: &mut ByteTargetWriter<W>) -> Result<u64, Error> {
    out.write_u16(0xFFFE).map_err(io_err)?;
    out.write_u16(element).map_err(io_err)?;
    out.write_u32(0).map_err(io_err)?;
    Ok(8)
}

/// Streams a buffer, byte-swapping `unit`-wide groups when the target byte
/// order differs from the native one.
fn write_buffer<W: Write>(
    out: &mut ByteTargetWriter<W>,
    buffer: &dyn ByteBuffer,
    unit: usize,
    target: Endian,
) -> Result<u64, Error> {
    let native = if cfg!(target_endian = "big") { Endian::Big } else { Endian::Little };
    if unit <= 1 || target == native {
        return out.write_buffer(buffer).map_err(io_err);
    }
    const CHUNK: u64 = 1024 * 1024;
    let mut written = 0u64;
    let mut offset = 0u64;
    let mut chunk = vec![0u8; CHUNK as usize];
    while offset < buffer.size() {
        let take = (buffer.size() - offset).min(CHUNK);
        // whole units only, so the swap stays aligned across chunks
        let take = take - take % unit as u64;
        let take = if take == 0 { buffer.size() - offset } else { take };
        let slice = &mut chunk[..take as usize];
        buffer.get_byte_range(offset, slice);
        Endian::swap_buffer(slice, unit);
        out.write_bytes(slice).map_err(io_err)?;
        offset += take;
        written += take;
    }
    Ok(written)
}

fn io_err(e: std::io::Error) -> Error {
    Error::Reader(crate::error::ReaderError::Other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::dataset_builder::parse_dataset_bytes;
    use crate::model::buffer::MemoryBuffer;
    use crate::model::transfer_syntax::syntaxes;
    use std::sync::Arc;

    fn le() -> Arc<TransferSyntax> {
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone()
    }

    fn implicit() -> Arc<TransferSyntax> {
        syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone()
    }

    fn be() -> Arc<TransferSyntax> {
        syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone()
    }

    fn encode(ds: &Dataset, syntax: &TransferSyntax, options: WriteOptions) -> Vec<u8> {
        let mut out = Vec::new();
        DicomWriter::new(options).write_dataset(ds, syntax, &mut out).unwrap();
        out
    }

    fn sample() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::ACCESSION_NUMBER, Vr::Sh, "ACC1");
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds.insert(crate::model::element::DicomElement::new_bytes(
            Tag::ROWS,
            Vr::Us,
            Arc::new(MemoryBuffer::from(vec![0x00, 0x02])),
        ))
        .unwrap();
        ds
    }

    #[test]
    fn leaf_headers_explicit_and_implicit() {
        let ds = sample();
        let explicit = encode(&ds, &le(), WriteOptions::default());
        // (0008,0050) SH "ACC1": 4 tag + 2 vr + 2 len + 4 value
        assert_eq!(&explicit[..10], &[0x08, 0x00, 0x50, 0x00, b'S', b'H', 4, 0, b'A', b'C']);
        let implicit = encode(&ds, &implicit(), WriteOptions::default());
        // implicit: 4 tag + 4 len, no VR bytes
        assert_eq!(&implicit[..8], &[0x08, 0x00, 0x50, 0x00, 4, 0, 0, 0]);
    }

    #[test]
    fn declared_length_matches_encoded_size() {
        let ds = sample();
        for syntax in [le(), implicit(), be()] {
            let bytes = encode(&ds, &syntax, WriteOptions::default());
            let declared = DicomWriter::default().dataset_length(&ds, &syntax).unwrap();
            assert_eq!(declared, bytes.len() as u64, "WR-02 must match WR-01 for {syntax:?}");
        }
    }

    #[test]
    fn round_trip_explicit_little_endian() {
        let ds = sample();
        let bytes = encode(&ds, &le(), WriteOptions::default());
        let back = parse_dataset_bytes(&bytes, &le()).unwrap();
        assert_eq!(back.get_str(Tag::PATIENT_NAME).unwrap(), "Doe^John");
        let rows: u16 = back.get(Tag::ROWS).unwrap();
        assert_eq!(rows, 512);
        assert_eq!(back, ds, "byte round-trip preserves the dataset");
    }

    #[test]
    fn round_trip_implicit_and_big_endian() {
        let ds = sample();
        for syntax in [implicit(), be()] {
            let bytes = encode(&ds, &syntax, WriteOptions::default());
            let back = parse_dataset_bytes(&bytes, &syntax).unwrap();
            assert_eq!(back, ds, "round-trip under {syntax:?}");
        }
    }

    #[test]
    fn undefined_sequence_writes_delimiters() {
        let mut ds = Dataset::new();
        let seq_tag = Tag::REFERENCED_IMAGE_SEQUENCE;
        let seq = ds.insert_sequence(seq_tag.clone());
        let mut item = Dataset::new();
        item.insert_str(Tag::REFERENCED_SOP_CLASS_UID, Vr::Ui, "1.2.3");
        seq.push(item);

        let bytes = encode(&ds, &le(), WriteOptions::default());
        // SQ header: tag + "SQ" + 2 reserved + FFFFFFFF
        assert_eq!(&bytes[4..6], b"SQ");
        assert_eq!(&bytes[8..12], &[0xFF, 0xFF, 0xFF, 0xFF], "undefined SQ length");
        // item header with undefined length
        assert_eq!(&bytes[12..16], &[0xFE, 0xFF, 0x00, 0xE0]);
        assert_eq!(&bytes[16..20], &[0xFF, 0xFF, 0xFF, 0xFF]);
        // ends with the sequence delimiter
        let tail = &bytes[bytes.len() - 8..];
        assert_eq!(tail, &[0xFE, 0xFF, 0xDD, 0xE0, 0, 0, 0, 0], "sequence delimiter");

        let back = parse_dataset_bytes(&bytes, &le()).unwrap();
        assert_eq!(back, ds);
    }

    #[test]
    fn explicit_length_sequence_round_trip() {
        let mut ds = Dataset::new();
        let seq = ds.insert_sequence(Tag::REFERENCED_IMAGE_SEQUENCE);
        let mut item = Dataset::new();
        item.insert_str(Tag::REFERENCED_SOP_CLASS_UID, Vr::Ui, "1.2.3");
        seq.push(item);

        let options = WriteOptions {
            explicit_length_sequences: true,
            explicit_length_sequence_items: true,
            keep_group_lengths: false,
        };
        let bytes = encode(&ds, &le(), options.clone());
        assert_ne!(&bytes[8..12], &[0xFF, 0xFF, 0xFF, 0xFF], "defined SQ length");
        assert_eq!(DicomWriter::new(options.clone()).dataset_length(&ds, &le()).unwrap(), bytes.len() as u64);
        let back = parse_dataset_bytes(&bytes, &le()).unwrap();
        assert_eq!(back, ds);
    }

    #[test]
    fn private_sequence_is_always_explicit_length() {
        let mut ds = Dataset::new();
        let tag = Tag::new(0x0029, 0x1001);
        let seq = ds.insert_sequence(tag.clone());
        let mut item = Dataset::new();
        item.insert_str(Tag::new(0x0029, 0x1010), Vr::Lo, "widget");
        seq.push(item);

        // even with the default (undefined) option, private SQs get a length
        let bytes = encode(&ds, &le(), WriteOptions::default());
        assert_ne!(&bytes[8..12], &[0xFF, 0xFF, 0xFF, 0xFF], "private SQ uses a defined length");
        let back = parse_dataset_bytes(&bytes, &le()).unwrap();
        assert_eq!(back, ds);
    }

    #[test]
    fn fragments_write_offset_table_and_delimiter() {
        let mut ds = Dataset::new();
        let frags = ds.insert_fragments(Tag::PIXEL_DATA, Vr::Ob);
        frags.push_fragment(Arc::new(MemoryBuffer::from(vec![0xAA, 0xBB])));
        frags.push_fragment(Arc::new(MemoryBuffer::from(vec![0xCC])));

        let bytes = encode(&ds, &le(), WriteOptions::default());
        let back = parse_dataset_bytes(&bytes, &le()).unwrap();
        match back.item(&Tag::PIXEL_DATA) {
            Some(DicomItem::Fragments(f)) => {
                assert_eq!(f.fragments().len(), 2);
                assert_eq!(f.offset_table(), &[0, 10], "offset table follows item headers + padding");
                assert_eq!(&*f.fragments()[0].data(), &[0xAA, 0xBB]);
                // the reader keeps the declared (padded) item length, so a
                // re-write is byte-identical
                assert_eq!(&*f.fragments()[1].data(), &[0xCC, 0x00]);
            }
            other => panic!("expected fragments, got {other:?}"),
        }
    }

    #[test]
    fn group_lengths_are_recalculated_or_removed() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");

        // default: no group length element on the wire
        let without = encode(&ds, &le(), WriteOptions::default());
        assert!(parse_dataset_bytes(&without, &le()).unwrap().item(&Tag::new(0x0010, 0x0000)).is_none());

        // keep_group_lengths: one recalculated element per group
        let options = WriteOptions { keep_group_lengths: true, ..Default::default() };
        let with = encode(&ds, &le(), options.clone());
        let back = parse_dataset_bytes(&with, &le()).unwrap();
        let gl: u32 = back.get(Tag::new(0x0010, 0x0000)).unwrap();
        let total = DicomWriter::new(options.clone()).dataset_length(&ds, &le()).unwrap();
        assert_eq!(gl as u64, total - 12, "group length covers the rest of the group");
        assert_eq!(total, with.len() as u64);
    }

    #[test]
    fn group_length_uses_target_byte_order() {
        // a big-endian dataset must write its UL group length big-endian too
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        let options = WriteOptions { keep_group_lengths: true, ..Default::default() };
        let bytes = encode(&ds, &be(), options);
        // tag(4) + "UL"(2) + len(2) then the value
        assert_eq!(&bytes[4..6], b"UL");
        let declared = u16::from_be_bytes([bytes[6], bytes[7]]) as usize;
        assert_eq!(declared, 4);
        let value = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        assert_eq!(value as usize, bytes.len() - 12, "value counts the remaining group bytes");
        assert_eq!(DicomWriter::new(WriteOptions { keep_group_lengths: true, ..Default::default() })
            .dataset_length(&ds, &be())
            .unwrap(), bytes.len() as u64);
    }

    #[test]
    fn long_value_demoted_to_un() {
        // CP-1066: a value above 0xFFFE under a 16-bit-length VR becomes UN
        let mut ds = Dataset::new();
        let big = vec![b'A'; 0x10000];
        ds.insert_str(Tag::ACCESSION_NUMBER, Vr::Sh, String::from_utf8(big).unwrap());
        let bytes = encode(&ds, &le(), WriteOptions::default());
        assert_eq!(&bytes[4..6], b"UN", "demoted to UN");
        assert_eq!(&bytes[6..8], &[0, 0], "UN uses the reserved form");
        assert_eq!(u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]), 0x10000);
        assert_eq!(DicomWriter::default().dataset_length(&ds, &le()).unwrap(), bytes.len() as u64);
    }

    #[test]
    fn odd_length_string_is_padded() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID");
        let bytes = encode(&ds, &le(), WriteOptions::default());
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 4, "padded to even length");
        assert_eq!(bytes[bytes.len() - 1], b' ', "space padding for string VRs");
    }

    #[test]
    fn charset_is_used_for_encoding() {
        // (0008,0005) selects the charset for character-encoded VRs
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SPECIFIC_CHARACTER_SET, Vr::Cs, "ISO_IR 192");
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "王^小明");
        let bytes = encode(&ds, &le(), WriteOptions::default());
        let back = parse_dataset_bytes(&bytes, &le()).unwrap();
        assert_eq!(back.get_str(Tag::PATIENT_NAME).unwrap(), "王^小明", "UTF-8 round-trip");
    }
}
