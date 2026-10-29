//! DICOM XML export (XML-01; docs/01-architecture.md §12).
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
//! `<InlineBinary>` (base64), sequences nest their items inside the same
//! `<DicomAttribute>` element, and encapsulated pixel data becomes numbered
//! `<Fragment>` elements.

use base64::Engine as _;
use radicom::model::dataset::Dataset;
use radicom::model::element::DicomElement;
use radicom::model::item::{DicomItem, DicomSequence};
use radicom::model::tag::Tag;
use radicom::model::vr::Vr;
use radicom::Error;
use std::fmt::Write as _;
/// Renders a dataset as an XML string (XML-01). A deferred value that cannot be
/// read is an error, not an empty value.
pub fn dataset_to_xml(dataset: &Dataset) -> Result<String, Error> {
    let mut out = String::with_capacity(1024);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<NativeDicomModel>\n");
    write_dataset(&mut out, dataset, 1)?;
    out.push_str("</NativeDicomModel>\n");
    Ok(out)
}

/// Writes one dataset's items at `depth`.
fn write_dataset(out: &mut String, dataset: &Dataset, depth: usize) -> Result<(), Error> {
    for item in dataset.items() {
        match item {
            DicomItem::Element(element) => write_element(out, element, depth)?,
            DicomItem::Sequence(sequence) => write_sequence(out, sequence, depth)?,
            // encapsulated pixel data exports as numbered <Fragment> elements,
            // which is what fo-dicom's DicomXML writer does too
            DicomItem::Fragments(fragments) => {
                write_fragments(out, fragments.tag(), fragments.vr(), fragments, depth)?
            }
        }
    }
    Ok(())
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// Writes a `<DicomAttribute>` for one element.
fn write_element(out: &mut String, element: &DicomElement, depth: usize) -> Result<(), Error> {
    indent(out, depth);
    write_attribute_open(out, element.tag(), element.vr());
    out.push('>');
    out.push('\n');

    let vr = element.vr();
    if is_binary(vr) {
        write_inline_binary(out, element, depth + 1)?;
    } else if vr == Vr::Sq {
        // an empty sequence element: no items
    } else {
        write_values(out, element, depth + 1)?;
    }

    indent(out, depth);
    out.push_str("</DicomAttribute>\n");
    Ok(())
}

/// `<DicomAttribute tag=.. vr=.. keyword=..>` (no closing bracket).
fn write_attribute_open(out: &mut String, tag: &Tag, vr: Vr) {
    let keyword = radicom::default_dictionary().entry(tag).keyword.to_string();
    let _ = write!(
        out,
        "<DicomAttribute tag=\"{}\" vr=\"{}\"",
        tag.to_json_string(),
        vr.code()
    );
    if !keyword.is_empty() {
        let _ = write!(out, " keyword=\"{}\"", escape(&keyword));
    }
    // private creators carry the creator name as an attribute
    if let Some(creator) = tag.private_creator() {
        let _ = write!(out, " privateCreator=\"{}\"", escape(creator.as_str()));
    }
}

fn write_values(out: &mut String, element: &DicomElement, depth: usize) -> Result<(), Error> {
    let vr = element.vr();
    if vr == Vr::Pn {
        write_person_name(out, element, depth)?;
        return Ok(());
    }
    // byte-backed non-binary VRs (parsed US/AT/etc.) render through their text
    let text = text_form(element, vr)?;
    if text.is_empty() {
        return Ok(());
    }
    for (index, part) in text.split('\\').enumerate() {
        indent(out, depth);
        // every component keeps its position, including empty ones: the
        // `number` attribute is the value index (PS3.19 A.1), so dropping an
        // empty component would renumber the following values (FEAT-18)
        let _ = writeln!(
            out,
            "<Value number=\"{}\">{}</Value>",
            index + 1,
            escape(part)
        );
    }
    Ok(())
}
/// The text of every value component, `\`-joined (§12.1).
fn text_form(element: &DicomElement, vr: Vr) -> Result<String, Error> {
    match element.value().as_buffer() {
        // A byte-backed string VR (e.g. an unloaded BulkDataURI) is text, not a
        // sequence of one-byte units: decode it as a string.
        Some(raw) if vr.is_string() => {
            let bytes = raw.data()?;
            Ok(String::from_utf8_lossy(bytes.as_ref())
                .trim_end_matches(['\0', ' '])
                .to_string())
        }
        // parsed binary VRs: render each unit as text so XML stays readable
        Some(raw) => raw_units_text(raw, vr),
        None => Ok(element.string_value().to_string()),
    }
}

/// Text form of a byte-backed numeric VR (US/UL/SS/SL/FL/FD/AT).
fn raw_units_text(raw: &radicom::model::buffer::SharedBuffer, vr: Vr) -> Result<String, Error> {
    let bytes = raw.data()?;
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
            Vr::Sv if unit == 8 => i64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ])
            .to_string(),
            Vr::Uv if unit == 8 => u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ])
            .to_string(),
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
    Ok(parts.join("\\"))
}

/// Writes PN as the Native DICOM Model shape (§12.1, PS3.19 / PS3.18 F.3.1):
/// one `<PersonName number="n">` per value (VM `\`-separated), each carrying
/// `<Alphabetic>`/`<Ideographic>`/`<Phonetic>` groups of named component
/// elements `<FamilyName>`/`<GivenName>`/`<MiddleName>`/`<NamePrefix>`/`<NameSuffix>`.
fn write_person_name(out: &mut String, element: &DicomElement, depth: usize) -> Result<(), Error> {
    let text = match element.value().as_buffer() {
        Some(raw) => String::from_utf8_lossy(raw.data()?.as_ref()).into_owned(),
        None => element.string_value().to_string(),
    };
    const GROUPS: [&str; 3] = ["Alphabetic", "Ideographic", "Phonetic"];
    const COMPONENTS: [&str; 5] = [
        "FamilyName",
        "GivenName",
        "MiddleName",
        "NamePrefix",
        "NameSuffix",
    ];
    let mut number = 1;
    for value in text.split('\\') {
        indent(out, depth);
        let _ = writeln!(out, "<PersonName number=\"{number}\">");
        let mut group_iter = value.split('=');
        for group in GROUPS {
            let Some(group_text) = group_iter.next() else {
                break;
            };
            let parts: Vec<&str> = group_text.split('^').collect();
            if !parts.iter().any(|p| !p.is_empty()) {
                continue; // an absent group is omitted
            }
            indent(out, depth + 1);
            let _ = writeln!(out, "<{group}>");
            for (index, component) in COMPONENTS.iter().enumerate() {
                if let Some(part) = parts.get(index).filter(|p| !p.is_empty()) {
                    indent(out, depth + 2);
                    let _ = writeln!(out, "<{component}>{}</{component}>", escape(part));
                }
            }
            indent(out, depth + 1);
            let _ = writeln!(out, "</{group}>");
        }
        indent(out, depth);
        out.push_str("</PersonName>\n");
        number += 1;
    }
    Ok(())
}

fn write_inline_binary(
    out: &mut String,
    element: &DicomElement,
    depth: usize,
) -> Result<(), Error> {
    let Some(buffer) = element.value().as_buffer() else {
        return Ok(());
    };
    if buffer.size() == 0 {
        return Ok(());
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(buffer.data()?.as_ref());
    indent(out, depth);
    let _ = writeln!(out, "<InlineBinary>{encoded}</InlineBinary>");
    Ok(())
}

/// Writes a sequence attribute with its items nested as `<Item>` (§12.1).
fn write_sequence(out: &mut String, sequence: &DicomSequence, depth: usize) -> Result<(), Error> {
    indent(out, depth);
    write_attribute_open(out, sequence.tag(), Vr::Sq);
    out.push_str(">\n");
    for (index, item) in sequence.items().iter().enumerate() {
        indent(out, depth + 1);
        let _ = writeln!(out, "<Item number=\"{}\">", index + 1);
        write_dataset(out, item, depth + 2)?;
        indent(out, depth + 1);
        out.push_str("</Item>\n");
    }
    indent(out, depth);
    out.push_str("</DicomAttribute>\n");
    Ok(())
}

/// Fragment sequences are not representable; render what the bytes carry so
/// the export does not silently drop it (§12.1 rejects them on the JSON side).
fn write_fragments(
    out: &mut String,
    tag: &Tag,
    vr: Vr,
    fragments: &radicom::model::item::DicomFragmentSequence,
    depth: usize,
) -> Result<(), Error> {
    indent(out, depth);
    write_attribute_open(out, tag, vr);
    out.push_str(">\n");
    for (index, fragment) in fragments.fragments().iter().enumerate() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(fragment.data()?.as_ref());
        // fo-dicom DicomXML wraps each fragment in <Fragment number="n">: the
        // fragment index must survive the export (FEAT-17)
        indent(out, depth + 1);
        let _ = writeln!(out, "<Fragment number=\"{}\">", index + 1);
        indent(out, depth + 2);
        let _ = writeln!(out, "<InlineBinary>{encoded}</InlineBinary>");
        indent(out, depth + 1);
        out.push_str("</Fragment>\n");
    }
    indent(out, depth);
    out.push_str("</DicomAttribute>\n");
    Ok(())
}

/// Binary VRs carried as base64 (§12.1).
fn is_binary(vr: Vr) -> bool {
    matches!(
        vr,
        Vr::Ob | Vr::Od | Vr::Of | Vr::Ol | Vr::Ov | Vr::Ow | Vr::Un
    )
}

/// Escapes the five XML metacharacters and drops characters XML 1.0 does not
/// allow (C0 controls other than tab/LF/CR, and the non-characters FFFE/FFFF),
/// so a leniently parsed value cannot make the document ill-formed (FEAT-18).
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 Char production: #x9 | #xA | #xD | [#x20-#xD7FF] | ...
            '\u{9}' | '\u{A}' | '\u{D}' => out.push(ch),
            ch if (ch as u32) < 0x20 || ch == '\u{FFFE}' || ch == '\u{FFFF}' => {
                out.push('\u{FFFD}')
            }
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
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "12345");
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds
    }

    #[test]
    fn the_document_has_the_native_model_root() {
        let xml = dataset_to_xml(&dataset()).unwrap();
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("<NativeDicomModel>"));
        assert!(xml.trim_end().ends_with("</NativeDicomModel>"));
    }

    #[test]
    fn an_element_carries_tag_vr_keyword_and_value() {
        let xml = dataset_to_xml(&dataset()).unwrap();
        assert!(
            xml.contains("<DicomAttribute tag=\"00100020\" vr=\"LO\" keyword=\"PatientID\">"),
            "{xml}"
        );
        assert!(xml.contains("<Value number=\"1\">12345</Value>"));
        assert!(xml.contains("</DicomAttribute>"));
    }

    #[test]
    fn a_person_name_expands_to_named_component_groups() {
        let xml = dataset_to_xml(&dataset()).unwrap();
        assert!(xml.contains("<PersonName number=\"1\">"), "{xml}");
        assert!(xml.contains("<Alphabetic>"), "{xml}");
        assert!(xml.contains("<FamilyName>Doe</FamilyName>"), "{xml}");
        assert!(xml.contains("<GivenName>John</GivenName>"), "{xml}");
    }

    #[test]
    fn a_multi_valued_person_name_becomes_numbered_person_name_elements() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John\\Roe^Jane");
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(
            xml.contains("<PersonName number=\"1\">") && xml.contains("<PersonName number=\"2\">"),
            "{xml}"
        );
        assert!(xml.contains("<FamilyName>Doe</FamilyName>"), "{xml}");
        assert!(
            xml.contains("<FamilyName>Roe</FamilyName>"),
            "the second value is not merged: {xml}"
        );
    }

    #[test]
    fn a_multi_value_element_numbers_each_component() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::IMAGE_TYPE, Vr::Cs, "ORIGINAL\\PRIMARY\\AXIAL");
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(
            xml.contains("<Value number=\"1\">ORIGINAL</Value>"),
            "{xml}"
        );
        assert!(xml.contains("<Value number=\"2\">PRIMARY</Value>"), "{xml}");
        assert!(xml.contains("<Value number=\"3\">AXIAL</Value>"), "{xml}");
    }

    #[test]
    fn a_binary_value_is_inline_base64() {
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            Arc::new(radicom::model::buffer::MemoryBuffer::from(vec![
                1u8, 2, 3, 4,
            ])),
        )));
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(
            xml.contains("<InlineBinary>AQIDBA==</InlineBinary>"),
            "{xml}"
        );
    }

    #[test]
    fn a_sequence_nests_its_items() {
        let mut ds = Dataset::new();
        let sequence = ds.insert_sequence(Tag::REFERENCED_IMAGE_SEQUENCE);
        let mut item = Dataset::new();
        item.insert_str(
            Tag::REFERENCED_SOP_CLASS_UID,
            Vr::Ui,
            "1.2.840.10008.5.1.4.1.1.2",
        );
        sequence.push(item);

        let xml = dataset_to_xml(&ds).unwrap();
        assert!(xml.contains("vr=\"SQ\""), "{xml}");
        assert!(xml.contains("<Item number=\"1\">"), "{xml}");
        assert!(xml.contains("<Value number=\"1\">1.2.840.10008.5.1.4.1.1.2</Value>"));
        assert!(xml.contains("</Item>"));
    }

    #[test]
    fn xml_metacharacters_are_escaped() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::INSTITUTION_ADDRESS, Vr::St, "a & b < c > d");
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(xml.contains("a &amp; b &lt; c &gt; d"), "{xml}");
        assert!(!xml.contains("a & b"), "the raw ampersand must not appear");
    }

    #[test]
    fn a_parsed_binary_integer_renders_as_text() {
        // a US element parsed from the wire is byte-backed
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::ROWS,
            Vr::Us,
            Arc::new(radicom::model::buffer::MemoryBuffer::from(
                512u16.to_le_bytes().to_vec(),
            )),
        )));
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(xml.contains("<Value number=\"1\">512</Value>"), "{xml}");
    }

    #[test]
    fn an_empty_value_writes_no_value_elements() {
        let mut ds = Dataset::new();
        ds.insert_empty(Tag::PATIENT_BIRTH_DATE, Vr::Da);
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(
            !xml.contains("<Value"),
            "an empty element carries no Value: {xml}"
        );
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
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "P&<1>");
        ds.insert_str(Tag::IMAGE_TYPE, Vr::Cs, "ORIGINAL\\PRIMARY");
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            Arc::new(radicom::model::buffer::MemoryBuffer::from(vec![
                1u8, 2, 3, 4,
            ])),
        )));
        let sequence = ds.insert_sequence(Tag::REFERENCED_IMAGE_SEQUENCE);
        let mut item = Dataset::new();
        item.insert_str(Tag::REFERENCED_SOP_CLASS_UID, Vr::Ui, "1.2.3");
        sequence.push(item);

        let xml = dataset_to_xml(&ds).unwrap();
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
        assert!(
            xml.contains("<InlineBinary>AQIDBA==</InlineBinary>"),
            "binary must appear: {xml}"
        );
        // the escaped ampersand must decode back to the original text
        assert!(
            xml.contains("P&amp;&lt;1&gt;"),
            "value metacharacters are escaped: {xml}"
        );
    }

    #[test]
    fn fragments_are_wrapped_and_numbered() {
        // FEAT-17: fo-dicom wraps every fragment in <Fragment number="n">.
        let mut ds = Dataset::new();
        let fragments = ds.insert_fragments(Tag::PIXEL_DATA, Vr::Ob);
        fragments.push_fragment(Arc::new(radicom::model::buffer::MemoryBuffer::from(vec![
            1u8, 2,
        ])));
        fragments.push_fragment(Arc::new(radicom::model::buffer::MemoryBuffer::from(vec![
            3u8,
        ])));

        let xml = dataset_to_xml(&ds).unwrap();
        assert!(xml.contains("<Fragment number=\"1\">"), "{xml}");
        assert!(xml.contains("<Fragment number=\"2\">"), "{xml}");
        assert!(xml.contains("<InlineBinary>AQI=</InlineBinary>"), "{xml}");
        assert!(xml.contains("<InlineBinary>Aw==</InlineBinary>"), "{xml}");
        assert_eq!(xml.matches("</Fragment>").count(), 2, "{xml}");
    }

    #[test]
    fn empty_value_components_keep_their_number() {
        // FEAT-18: dropping the empty middle component would renumber the rest.
        let mut ds = Dataset::new();
        ds.insert_str(Tag::IMAGE_TYPE, Vr::Cs, "A\\\\C");
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(xml.contains("<Value number=\"1\">A</Value>"), "{xml}");
        assert!(xml.contains("<Value number=\"2\"></Value>"), "{xml}");
        assert!(xml.contains("<Value number=\"3\">C</Value>"), "{xml}");
    }

    #[test]
    fn xml_invalid_control_characters_are_replaced() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::INSTITUTION_ADDRESS, Vr::St, "a\u{1}b");
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(!xml.contains('\u{1}'), "control char must not be emitted");
        assert!(xml.contains("a\u{FFFD}b"), "{xml}");
        // the document must stay well-formed
        use quick_xml::events::Event;
        use quick_xml::Reader;
        let mut reader = Reader::from_str(&xml);
        loop {
            if matches!(reader.read_event().expect("well-formed XML"), Event::Eof) {
                break;
            }
        }
    }

    #[test]
    fn an_empty_binary_value_carries_no_inline_binary() {
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            Arc::new(radicom::model::buffer::MemoryBuffer::from(Vec::new())),
        )));
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(!xml.contains("<InlineBinary>"), "{xml}");
        assert!(xml.contains("vr=\"OB\""), "{xml}");
    }

    #[test]
    fn a_person_name_skips_empty_groups_and_keeps_phonetic() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John==Do^J");
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(xml.contains("<Alphabetic>"), "{xml}");
        assert!(!xml.contains("<Ideographic>"), "empty group omitted: {xml}");
        assert!(xml.contains("<Phonetic>"), "{xml}");
    }

    #[test]
    fn parsed_numeric_vrs_render_their_unit_text() {
        // covers SS/SL/SV/UV/FL/FD/AT branches of raw_units_text
        let cases: Vec<(Tag, Vr, Vec<u8>, &str)> = vec![
            (
                Tag::new(0x0011, 0x0001),
                Vr::Ss,
                (-2i16).to_le_bytes().to_vec(),
                "-2",
            ),
            (
                Tag::new(0x0011, 0x0002),
                Vr::Sl,
                (-70000i32).to_le_bytes().to_vec(),
                "-70000",
            ),
            (
                Tag::new(0x0011, 0x0003),
                Vr::Sv,
                (-5i64).to_le_bytes().to_vec(),
                "-5",
            ),
            (
                Tag::new(0x0011, 0x0004),
                Vr::Uv,
                (9u64).to_le_bytes().to_vec(),
                "9",
            ),
            (
                Tag::new(0x0011, 0x0005),
                Vr::Fl,
                (1.5f32).to_le_bytes().to_vec(),
                "1.5",
            ),
            (
                Tag::new(0x0011, 0x0006),
                Vr::Fd,
                (2.25f64).to_le_bytes().to_vec(),
                "2.25",
            ),
            (
                Tag::QUERY_RETRIEVE_LEVEL,
                Vr::At,
                {
                    let mut b = Vec::new();
                    b.extend_from_slice(&0x0010u16.to_le_bytes());
                    b.extend_from_slice(&0x0010u16.to_le_bytes());
                    b
                },
                "00100010",
            ),
        ];
        for (tag, vr, bytes, expected) in cases {
            let mut ds = Dataset::new();
            ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
                tag,
                vr,
                Arc::new(radicom::model::buffer::MemoryBuffer::from(bytes)),
            )));
            let xml = dataset_to_xml(&ds).unwrap();
            assert!(
                xml.contains(expected),
                "{vr:?} must render {expected:?}: {xml}"
            );
        }
    }

    #[test]
    fn a_private_tag_carries_its_creator_attribute() {
        let creator = Arc::new(radicom::model::private_creator::PrivateCreator::new(
            "ACME 1.2",
        ));
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_str(
            Tag::private(0x0029, 0x1001, creator),
            Vr::Lo,
            "widget",
        )));
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(
            xml.contains("privateCreator=\"ACME 1.2\""),
            "creator name is exported: {xml}"
        );
    }

    #[test]
    fn an_sq_element_without_items_writes_no_children() {
        // an SQ stored as a length-0 element (not a DicomSequence)
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_empty(
            Tag::REFERENCED_IMAGE_SEQUENCE,
            Vr::Sq,
        )));
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(xml.contains("vr=\"SQ\""), "{xml}");
        assert!(!xml.contains("<Item"), "{xml}");
        assert!(!xml.contains("<Value"), "{xml}");
    }

    #[test]
    fn a_short_byte_backed_value_keeps_whole_units_only() {
        // 3 bytes of US: the trailing partial unit is dropped, never indexed
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::ROWS,
            Vr::Us,
            Arc::new(radicom::model::buffer::MemoryBuffer::from(vec![
                0x01, 0x00, 0x02,
            ])),
        )));
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(xml.contains("<Value number=\"1\">1</Value>"), "{xml}");
        assert!(
            !xml.contains("513"),
            "the partial unit is not decoded: {xml}"
        );
    }

    #[test]
    fn a_byte_backed_string_vr_falls_back_to_lossy_text() {
        // a byte-backed LO has unit size 1; raw_units_text uses the string branch
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PATIENT_ID,
            Vr::Lo,
            Arc::new(radicom::model::buffer::MemoryBuffer::from(
                b"PID-9".to_vec(),
            )),
        )));
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(xml.contains("<Value number=\"1\">PID-9</Value>"), "{xml}");
    }

    #[test]
    fn a_byte_backed_person_name_is_decoded_for_export() {
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::PATIENT_NAME,
            Vr::Pn,
            Arc::new(radicom::model::buffer::MemoryBuffer::from(
                b"Doe^Jane".to_vec(),
            )),
        )));
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(xml.contains("<FamilyName>Doe</FamilyName>"), "{xml}");
        assert!(xml.contains("<GivenName>Jane</GivenName>"), "{xml}");
    }

    #[test]
    fn quotes_apostrophes_and_whitespace_are_escaped_or_preserved() {
        let mut ds = Dataset::new();
        ds.insert_str(
            Tag::INSTITUTION_ADDRESS,
            Vr::St,
            "he said \"hi\" & 'bye'\tend",
        );
        let xml = dataset_to_xml(&ds).unwrap();
        assert!(xml.contains("&quot;hi&quot;"), "{xml}");
        assert!(xml.contains("&apos;bye&apos;"), "{xml}");
        assert!(xml.contains('\t'), "tab is valid XML text: {xml}");
    }
}
