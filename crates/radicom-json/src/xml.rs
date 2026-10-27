//! DICOM XML export (XML-01; docs/08-serialization-media.md §2).
//!
//! One-way export to fo-dicom's `NativeDicomModel` shape:
//!
//! ```xml
//! <NativeDicomModel>
//!   <DicomAttribute tag="00100020" vr="LO" keyword="PatientID">
//!     <Value number="1">12345</Value>
//!   </DicomAttribute>
//!   ...
//! </NativeDicomModel>
//! ```
//!
//! There is no reader: XML is a debugging/interchange export only. PN expands
//! to `Alphabetic`/`Ideographic`/`Phonetic` child elements, binary VRs use
//! `<InlineBinary>` (base64), and sequences nest their items inside the same
//! `<DicomAttribute>` element.

use base64::Engine as _;
use radicom::model::dataset::Dataset;
use radicom::model::element::DicomElement;
use radicom::model::item::{DicomItem, DicomSequence};
use radicom::model::tag::Tag;
use radicom::model::vr::Vr;
use std::fmt::Write as _;
/// Renders a dataset as an XML string (XML-01).
pub fn dataset_to_xml(dataset: &Dataset) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<NativeDicomModel>\n");
    write_dataset(&mut out, dataset, 1);
    out.push_str("</NativeDicomModel>\n");
    out
}

/// Writes one dataset's items at `depth`.
fn write_dataset(out: &mut String, dataset: &Dataset, depth: usize) {
    for item in dataset.items() {
        match item {
            DicomItem::Element(element) => write_element(out, element, depth),
            DicomItem::Sequence(sequence) => write_sequence(out, sequence, depth),
            // fragment sequences are not representable in the XML model
            DicomItem::Fragments(fragments) => write_fragments(out, fragments.tag(), fragments.vr(), fragments, depth),
        }
    }
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// Writes a `<DicomAttribute>` for one element.
fn write_element(out: &mut String, element: &DicomElement, depth: usize) {
    indent(out, depth);
    write_attribute_open(out, element.tag(), element.vr());
    out.push('>');
    out.push('\n');

    let vr = element.vr();
    if is_binary(vr) {
        write_inline_binary(out, element, depth + 1);
    } else if vr == Vr::Sq {
        // an empty sequence element: no items
    } else {
        write_values(out, element, depth + 1);
    }

    indent(out, depth);
    out.push_str("</DicomAttribute>\n");
}

/// `<DicomAttribute tag=.. vr=.. keyword=..>` (no closing bracket).
fn write_attribute_open(out: &mut String, tag: &Tag, vr: Vr) {
    let keyword = radicom::default_dictionary().entry(tag).keyword.to_string();
    let _ = write!(out, "<DicomAttribute tag=\"{}\" vr=\"{}\"", tag.to_json_string(), vr.code());
    if !keyword.is_empty() {
        let _ = write!(out, " keyword=\"{}\"", escape(&keyword));
    }
    // private creators carry the creator name as an attribute
    if let Some(creator) = tag.private_creator() {
        let _ = write!(out, " privateCreator=\"{}\"", escape(creator.as_str()));
    }
}

fn write_values(out: &mut String, element: &DicomElement, depth: usize) {
    let vr = element.vr();
    if vr == Vr::Pn {
        write_person_name(out, element.string_value(), depth);
        return;
    }
    // byte-backed non-binary VRs (parsed US/AT/etc.) render through their text
    let text = text_form(element, vr);
    let mut number = 1;
    for part in text.split('\\') {
        if part.is_empty() {
            continue;
        }
        indent(out, depth);
        let _ = writeln!(out, "<Value number=\"{number}\">{}</Value>", escape(part));
        number += 1;
    }
}

/// The text of every value component, `\`-joined (§2).
fn text_form(element: &DicomElement, vr: Vr) -> String {
    if let Some(raw) = element.value().as_buffer() {
        // parsed binary VRs: render each unit as text so XML stays readable
        return if is_binary(vr) {
            String::new()
        } else {
            raw_units_text(raw, vr)
        };
    }
    element.string_value().to_string()
}

/// Text form of a byte-backed numeric VR (US/UL/SS/SL/FL/FD/AT).
fn raw_units_text(raw: &radicom::model::buffer::SharedBuffer, vr: Vr) -> String {
    let bytes = raw.data();
    let bytes: &[u8] = bytes.as_ref();
    let unit = vr.unit_size().max(1);
    let mut parts = Vec::new();
    for chunk in bytes.chunks(unit) {
        if chunk.len() < unit {
            break;
        }
        let text = match vr {
            Vr::Us => u16::from_le_bytes([chunk[0], chunk[1]]).to_string(),
            Vr::Ss => i16::from_le_bytes([chunk[0], chunk[1]]).to_string(),
            Vr::Ul => u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).to_string(),
            Vr::Sl => i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).to_string(),
            Vr::Fl => f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).to_string(),
            Vr::Fd => f64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ])
            .to_string(),
            Vr::At if unit == 4 => format!(
                "{:04X}{:04X}",
                u16::from_le_bytes([chunk[0], chunk[1]]),
                u16::from_le_bytes([chunk[2], chunk[3]])
            ),
            _ => String::from_utf8_lossy(chunk).into_owned(),
        };
        parts.push(text);
    }
    parts.join("\\")
}

/// Writes PN groups as `Alphabetic`/`Ideographic`/`Phonetic` child elements (§2).
fn write_person_name(out: &mut String, text: &str, depth: usize) {
    // the model keeps PN groups `=`- or `\`-separated; the three-component
    // form the element stores is `Alphabetic=Ideographic=Phonetic`
    let mut groups = text.split('=');
    let components = ["Alphabetic", "Ideographic", "Phonetic"];
    for component in components {
        if let Some(value) = groups.next() {
            if value.is_empty() {
                continue;
            }
            indent(out, depth);
            let _ = write!(out, "<PersonName group=\"{component}\">");
            // a PN value component uses `^` for separator inside the file
            let _ = writeln!(out, "{}</PersonName>", escape(value));
        }
    }
}

fn write_inline_binary(out: &mut String, element: &DicomElement, depth: usize) {
    let Some(buffer) = element.value().as_buffer() else {
        return;
    };
    if buffer.size() == 0 {
        return;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(buffer.data().as_ref());
    indent(out, depth);
    let _ = writeln!(out, "<InlineBinary>{encoded}</InlineBinary>");
}

/// Writes a sequence attribute with its items nested as `<Item>` (§2).
fn write_sequence(out: &mut String, sequence: &DicomSequence, depth: usize) {
    indent(out, depth);
    write_attribute_open(out, sequence.tag(), Vr::Sq);
    out.push_str(">\n");
    for item in sequence.items() {
        indent(out, depth + 1);
        out.push_str("<Item>\n");
        write_dataset(out, item, depth + 2);
        indent(out, depth + 1);
        out.push_str("</Item>\n");
    }
    indent(out, depth);
    out.push_str("</DicomAttribute>\n");
}

/// Fragment sequences are not representable; render what the bytes carry so
/// the export does not silently drop it (§1.1 rejects them on the JSON side).
fn write_fragments(
    out: &mut String,
    tag: &Tag,
    vr: Vr,
    fragments: &radicom::model::item::DicomFragmentSequence,
    depth: usize,
) {
    indent(out, depth);
    write_attribute_open(out, tag, vr);
    out.push_str(">\n");
    for fragment in fragments.fragments() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(fragment.data().as_ref());
        indent(out, depth + 1);
        let _ = writeln!(out, "<InlineBinary>{encoded}</InlineBinary>");
    }
    indent(out, depth);
    out.push_str("</DicomAttribute>\n");
}

/// Binary VRs carried as base64 (§2).
fn is_binary(vr: Vr) -> bool {
    matches!(vr, Vr::Ob | Vr::Od | Vr::Of | Vr::Ol | Vr::Ov | Vr::Ow | Vr::Un)
}

/// Escapes the five XML metacharacters.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use radicom::model::dataset::Dataset;
    use radicom::model::item::DicomItem;
    use radicom::model::tag::Tag;
    use radicom::model::vr::Vr;
    use std::sync::Arc;

    fn dataset() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0010, 0x0020), Vr::Lo, "12345");
        ds.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Doe^John");
        ds
    }

    #[test]
    fn the_document_has_the_native_model_root() {
        let xml = dataset_to_xml(&dataset());
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("<NativeDicomModel>"));
        assert!(xml.trim_end().ends_with("</NativeDicomModel>"));
    }

    #[test]
    fn an_element_carries_tag_vr_keyword_and_value() {
        let xml = dataset_to_xml(&dataset());
        assert!(xml.contains("<DicomAttribute tag=\"00100020\" vr=\"LO\" keyword=\"PatientID\">"), "{xml}");
        assert!(xml.contains("<Value number=\"1\">12345</Value>"));
        assert!(xml.contains("</DicomAttribute>"));
    }

    #[test]
    fn a_person_name_expands_to_groups() {
        let xml = dataset_to_xml(&dataset());
        assert!(xml.contains("<PersonName group=\"Alphabetic\">Doe^John</PersonName>"), "{xml}");
    }

    #[test]
    fn a_multi_value_element_numbers_each_component() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0008, 0x0008), Vr::Cs, "ORIGINAL\\PRIMARY\\AXIAL");
        let xml = dataset_to_xml(&ds);
        assert!(xml.contains("<Value number=\"1\">ORIGINAL</Value>"), "{xml}");
        assert!(xml.contains("<Value number=\"2\">PRIMARY</Value>"), "{xml}");
        assert!(xml.contains("<Value number=\"3\">AXIAL</Value>"), "{xml}");
    }

    #[test]
    fn a_binary_value_is_inline_base64() {
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::new(0x7FE0, 0x0010),
            Vr::Ob,
            Arc::new(radicom::model::buffer::MemoryBuffer::from(vec![1u8, 2, 3, 4])),
        )));
        let xml = dataset_to_xml(&ds);
        assert!(xml.contains("<InlineBinary>AQIDBA==</InlineBinary>"), "{xml}");
    }

    #[test]
    fn a_sequence_nests_its_items() {
        let mut ds = Dataset::new();
        let sequence = ds.insert_sequence(Tag::new(0x0008, 0x1140));
        let mut item = Dataset::new();
        item.insert_str(Tag::new(0x0008, 0x1150), Vr::Ui, "1.2.840.10008.5.1.4.1.1.2");
        sequence.push(item);

        let xml = dataset_to_xml(&ds);
        assert!(xml.contains("vr=\"SQ\""), "{xml}");
        assert!(xml.contains("<Item>"), "{xml}");
        assert!(xml.contains("<Value number=\"1\">1.2.840.10008.5.1.4.1.1.2</Value>"));
        assert!(xml.contains("</Item>"));
    }

    #[test]
    fn xml_metacharacters_are_escaped() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0008, 0x0081), Vr::St, "a & b < c > d");
        let xml = dataset_to_xml(&ds);
        assert!(xml.contains("a &amp; b &lt; c &gt; d"), "{xml}");
        assert!(!xml.contains("a & b"), "the raw ampersand must not appear");
    }

    #[test]
    fn a_parsed_binary_integer_renders_as_text() {
        // a US element parsed from the wire is byte-backed
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::new(0x0028, 0x0010),
            Vr::Us,
            Arc::new(radicom::model::buffer::MemoryBuffer::from(512u16.to_le_bytes().to_vec())),
        )));
        let xml = dataset_to_xml(&ds);
        assert!(xml.contains("<Value number=\"1\">512</Value>"), "{xml}");
    }

    #[test]
    fn an_empty_value_writes_no_value_elements() {
        let mut ds = Dataset::new();
        ds.insert_empty(Tag::new(0x0010, 0x0030), Vr::Da);
        let xml = dataset_to_xml(&ds);
        assert!(!xml.contains("<Value"), "an empty element carries no Value: {xml}");
        assert!(xml.contains("</DicomAttribute>"));
    }

    /// The export must be well-formed XML, not just well-formed-looking text:
    /// parse it back and walk the element tree (docs §8: XML snapshot tests).
    #[test]
    fn the_export_parses_as_well_formed_xml() {
        use quick_xml::events::Event;
        use quick_xml::Reader;

        // a dataset exercising every carrier: string, multi-value, PN, SQ, binary
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0010, 0x0020), Vr::Lo, "P&<1>");
        ds.insert_str(Tag::new(0x0008, 0x0008), Vr::Cs, "ORIGINAL\\PRIMARY");
        ds.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Doe^John");
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::new(0x7FE0, 0x0010),
            Vr::Ob,
            Arc::new(radicom::model::buffer::MemoryBuffer::from(vec![1u8, 2, 3, 4])),
        )));
        let sequence = ds.insert_sequence(Tag::new(0x0008, 0x1140));
        let mut item = Dataset::new();
        item.insert_str(Tag::new(0x0008, 0x1150), Vr::Ui, "1.2.3");
        sequence.push(item);

        let xml = dataset_to_xml(&ds);
        let mut reader = Reader::from_str(&xml);
        reader.config_mut().trim_text(false);

        let mut depth = 0i32;
        let mut saw_root = false;
        let mut saw_item = false;
        let mut saw_pn_group = false;
        loop {
            match reader.read_event().expect("well-formed XML") {
                Event::Start(start) => {
                    let name = String::from_utf8_lossy(start.name().as_ref()).into_owned();
                    if name == "NativeDicomModel" {
                        saw_root = true;
                    }
                    if name == "Item" {
                        saw_item = true;
                    }
                    if name == "PersonName" {
                        saw_pn_group = true;
                    }
                    depth += 1;
                }
                Event::End(_) => depth -= 1,
                Event::Text(_) => {}
                Event::Eof => break,
                _ => {}
            }
            assert!(depth >= 0, "closing tag without an opener");
        }

        assert_eq!(depth, 0, "every tag must close");
        assert!(saw_root, "the NativeDicomModel root must parse");
        assert!(saw_item, "the sequence item must parse");
        assert!(saw_pn_group, "PN groups must parse as elements");
        assert!(xml.contains("<InlineBinary>AQIDBA==</InlineBinary>"), "binary must appear: {xml}");
        // the escaped ampersand must decode back to the original text
        assert!(xml.contains("P&amp;&lt;1&gt;"), "value metacharacters are escaped: {xml}");
    }
}
