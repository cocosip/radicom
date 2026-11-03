//! Dataset dump formatting (INF-04; docs/02-development-guide.md §2 INF-04, INF-07).
//!
//! Renders a `Dataset` as indented text — tag, VR, dictionary keyword and value
//! — with truncation controls. This is the library half of the `radicom-dump`
//! CLI (INF-07), mirroring fo-dicom's `FO-DICOM.Dump`.

use crate::io::file::DicomFile;
use crate::model::dataset::Dataset;
use crate::model::dictionary::{dictionary, Dictionary};
use crate::model::element::{DicomElement, ElementValue};
use crate::model::item::DicomItem;
use crate::model::tag::Tag;
use crate::model::uid::Uid;
use crate::model::vr::Vr;

/// Dump formatting options (INF-04).
#[derive(Debug, Clone)]
pub struct DumpOptions {
    /// Maximum characters of a string value before it is elided with `…`.
    pub max_value_chars: usize,
    /// Maximum bytes of binary data shown in the hex preview.
    pub max_binary_bytes: usize,
    /// Indent width per sequence level.
    pub indent: usize,
    /// Include private elements (odd groups).
    pub show_private: bool,
}

impl Default for DumpOptions {
    fn default() -> Self {
        DumpOptions {
            max_value_chars: 64,
            max_binary_bytes: 16,
            indent: 2,
            show_private: true,
        }
    }
}

/// Formats a dataset (and all nested sequences) as a multi-line string (INF-04).
pub fn dump_dataset(dataset: &Dataset, options: &DumpOptions) -> String {
    let mut out = String::new();
    write_level(dataset, 0, options, dictionary(), &mut out);
    out
}

/// Formats a whole file: the file-meta group (0002) followed by the dataset,
/// the same walk fo-dicom's `WriteToString(DicomFile)` / `WriteToLog(DicomFile)`
/// performs (`FO-DICOM.Core/Log/Extensions.cs:18-40`).
pub fn dump_file(file: &DicomFile, options: &DumpOptions) -> String {
    let mut out = String::new();
    write_level(&file.file_meta, 0, options, dictionary(), &mut out);
    write_level(&file.dataset, 0, options, dictionary(), &mut out);
    out
}

/// Formats one element's line at `depth`.
///
/// The dictionary is resolved once per dump, and the tag is borrowed from the
/// item instead of cloned.
fn write_level(
    dataset: &Dataset,
    depth: usize,
    options: &DumpOptions,
    dictionary: &Dictionary,
    out: &mut String,
) {
    let pad = " ".repeat(depth * options.indent);
    for item in dataset.items() {
        let tag = item.tag();
        // skip private elements unless requested (allowed as a trust-boundary option)
        if !options.show_private && tag.group() % 2 == 1 {
            continue;
        }
        let name = dictionary.entry(tag).keyword;
        let tag_text = format!("({:04X},{:04X})", tag.group(), tag.element());
        match item {
            DicomItem::Element(e) => {
                let value = with_uid_name(e, render_value(e.value(), e.vr(), options));
                out.push_str(&format!(
                    "{pad}{tag_text} {:<2} {name:<30} {value} # {}\n",
                    e.vr().code(),
                    element_length(e.value())
                ));
            }
            DicomItem::Sequence(s) => {
                out.push_str(&format!(
                    "{pad}{tag_text} SQ {name:<30} ({} item(s))\n",
                    s.items().len()
                ));
                for (index, item_ds) in s.items().iter().enumerate() {
                    out.push_str(&format!(
                        "{pad}{}item #{index}\n",
                        " ".repeat(options.indent)
                    ));
                    write_level(item_ds, depth + 2, options, dictionary, out);
                }
            }
            DicomItem::Fragments(f) => {
                let sizes: Vec<String> = f
                    .fragments()
                    .iter()
                    .map(|fragment| fragment.size().to_string())
                    .collect();
                let detail = if sizes.is_empty() {
                    String::new()
                } else {
                    format!(": {} bytes", sizes.join(" + "))
                };
                out.push_str(&format!(
                    "{pad}{tag_text} {:<2} {name:<30} ({} fragment(s){detail})\n",
                    f.vr().code(),
                    f.fragments().len()
                ));
            }
        }
    }
}

/// Renders an element value with truncation (INF-04).
fn render_value(value: &ElementValue, vr: crate::model::vr::Vr, options: &DumpOptions) -> String {
    match value {
        ElementValue::Str(text) => {
            let text = text.as_str();
            if text.is_empty() {
                return "(empty)".to_string();
            }
            if text.chars().count() > options.max_value_chars {
                let truncated: String = text.chars().take(options.max_value_chars).collect();
                format!("{truncated}… ({} chars)", text.chars().count())
            } else {
                text.to_string()
            }
        }
        ElementValue::Bytes(buffer) => {
            let size = buffer.size();
            if size == 0 {
                return "(empty)".to_string();
            }
            // Never materialize a large on-demand value just to preview a few
            // bytes (review CORE-09): read only the preview range.
            let shown = size.min(options.max_binary_bytes as u64) as usize;
            let mut preview = vec![0u8; shown];
            let bytes: &[u8] = match buffer.as_slice() {
                Some(slice) => &slice[..shown.min(slice.len())],
                None => match buffer.get_byte_range(0, &mut preview) {
                    Ok(()) => &preview,
                    Err(e) => return format!("(unreadable: {e})"),
                },
            };
            if vr.is_string() {
                // string-typed Other values: show as text when printable, but
                // only for values already in memory
                if let Some(full) = buffer.as_slice() {
                    if let Ok(text) = std::str::from_utf8(full) {
                        let text = text.trim_end_matches('\0');
                        if text.is_empty() {
                            return "(empty)".to_string();
                        }
                        if text
                            .chars()
                            .take(options.max_value_chars)
                            .all(|c| !c.is_control() || c == '\n')
                        {
                            return render_str(text, options);
                        }
                    }
                }
            }
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
            let suffix = if size > shown as u64 {
                format!(" … ({size} bytes)")
            } else {
                String::new()
            };
            format!("[{}]{suffix}", hex.join(" "))
        }
    }
}

fn render_str(text: &str, options: &DumpOptions) -> String {
    if text.chars().count() > options.max_value_chars {
        let truncated: String = text.chars().take(options.max_value_chars).collect();
        format!("{truncated}… ({} chars)", text.chars().count())
    } else {
        text.to_string()
    }
}

/// The element's in-memory byte length (fo-dicom `DicomElement.Length`).
///
/// Text is measured as UTF-8 bytes: the formatter does not know the dataset's
/// character set, so this is exact for binary VRs and for ASCII text.
fn element_length(value: &ElementValue) -> u64 {
    match value {
        ElementValue::Str(text) => text.len() as u64,
        ElementValue::Bytes(buffer) => buffer.size(),
    }
}

/// Appends the UID registry name to a rendered UI value, as fo-dicom's dump
/// does (`FO-DICOM.Dump/DumpWalker.cs:53-61`).
fn with_uid_name(element: &DicomElement, rendered: String) -> String {
    if element.vr() != Vr::Ui {
        return rendered;
    }
    let Some(text) = element.value().as_str() else {
        return rendered;
    };
    let first = text.split('\\').next().unwrap_or("").trim();
    let Some(uid) = Uid::query(first) else {
        return rendered;
    };
    match uid.name() {
        Some(name) => format!("{rendered} ({name})"),
        None => rendered,
    }
}

/// A one-line summary of pixel data, for the CLI (INF-07).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelSummary {
    pub rows: u16,
    pub columns: u16,
    pub frames: u32,
    pub bits_allocated: u16,
    pub samples_per_pixel: u16,
    pub photometric_interpretation: String,
}

impl PixelSummary {
    pub fn one_line(&self) -> String {
        format!(
            "{}x{} × {} frame(s), {} bits, {} sample(s)/pixel, {}",
            self.columns,
            self.rows,
            self.frames,
            self.bits_allocated,
            self.samples_per_pixel,
            self.photometric_interpretation
        )
    }
}

/// Reads the pixel-data summary from a dataset, if it carries pixel data (INF-07).
pub fn pixel_summary(dataset: &Dataset) -> Option<PixelSummary> {
    let rows = dataset.try_get::<u16>(Tag::ROWS)?;
    let columns = dataset.try_get::<u16>(Tag::COLUMNS)?;
    let bits_allocated = dataset.try_get::<u16>(Tag::BITS_ALLOCATED).unwrap_or(0);
    let samples_per_pixel = dataset.try_get::<u16>(Tag::SAMPLES_PER_PIXEL).unwrap_or(1);
    let frames = dataset
        .try_get_str(Tag::NUMBER_OF_FRAMES)
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(1);
    let photometric = dataset
        .try_get_str(Tag::PHOTOMETRIC_INTERPRETATION)
        .unwrap_or_else(|| "?".to_string());
    Some(PixelSummary {
        rows,
        columns,
        frames,
        bits_allocated,
        samples_per_pixel,
        photometric_interpretation: photometric.trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::vr::Vr;
    use std::sync::Arc;

    fn sample_dataset() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "DOE^JOHN");
        ds.insert_str(Tag::MODALITY, Vr::Cs, "CT");
        let mut item = Dataset::new();
        item.insert_str(Tag::CODE_VALUE, Vr::Sh, "1");
        ds.insert_sequence(Tag::REFERENCED_IMAGE_SEQUENCE)
            .push(item);
        ds
    }

    #[test]
    fn a_dump_lists_tag_vr_keyword_and_value() {
        let out = dump_dataset(&sample_dataset(), &DumpOptions::default());
        assert!(out.contains("(0010,0010)"), "shows the tag");
        assert!(out.contains("PatientName"), "shows the dictionary keyword");
        assert!(out.contains("DOE^JOHN"), "shows the value");
        assert!(out.contains("(0008,0060)"), "shows the second element");
    }

    #[test]
    fn a_sequence_is_rendered_with_its_items() {
        let out = dump_dataset(&sample_dataset(), &DumpOptions::default());
        assert!(out.contains("SQ"), "marks the sequence");
        assert!(out.contains("item #0"), "shows the nested item");
        assert!(
            out.contains("ReferencedImageSequence") || out.contains("(0008,1140)"),
            "shows the SQ tag"
        );
    }

    #[test]
    fn long_values_are_truncated_to_the_limit() {
        // (e): the old form asserted only that the full value is absent,
        // which a dump that prints no values at all also satisfies. Assert the
        // elided prefix positively, and that a short value is printed in full.
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SERIES_DESCRIPTION, Vr::Lo, "SHORT");
        let long = format!("{}{}", "PREFIX-", "A".repeat(94));
        ds.insert_str(Tag::IMAGE_TYPE, Vr::Cs, long.clone());
        let options = DumpOptions {
            max_value_chars: 10,
            ..Default::default()
        };
        let out = dump_dataset(&ds, &options);

        assert!(
            out.contains("SHORT"),
            "a value under the limit is printed in full"
        );
        assert!(
            out.contains("PREFIX-"),
            "the printed prefix is the value's real start"
        );
        assert!(!out.contains(&long), "the full long value is not printed");
        assert!(out.contains('…'), "the elision is marked");
    }

    #[test]
    fn binary_data_shows_a_hex_preview() {
        let mut ds = Dataset::new();
        let bytes: Vec<u8> = (0u8..32).collect();
        ds.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                Arc::new(crate::model::buffer::MemoryBuffer::from(bytes)),
            ),
        ));
        let options = DumpOptions {
            max_binary_bytes: 4,
            ..Default::default()
        };
        let out = dump_dataset(&ds, &options);
        assert!(out.contains("00 01 02 03"), "shows the bounded hex preview");
        assert!(out.contains("32 bytes"), "reports the full size");
    }

    #[test]
    fn an_empty_value_is_labelled() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::MODALITY, Vr::Cs, "");
        let out = dump_dataset(&ds, &DumpOptions::default());
        assert!(out.contains("(empty)"), "empty values are labelled");
    }

    #[test]
    fn private_elements_can_be_hidden() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0009, 0x0010), Vr::Lo, "PRIVATE");
        let shown = dump_dataset(
            &ds,
            &DumpOptions {
                show_private: true,
                ..Default::default()
            },
        );
        assert!(shown.contains("PRIVATE"));
        let hidden = dump_dataset(
            &ds,
            &DumpOptions {
                show_private: false,
                ..Default::default()
            },
        );
        assert!(
            !hidden.contains("PRIVATE"),
            "private element is filtered out"
        );
    }

    #[test]
    fn byte_strings_and_fragments_are_rendered() {
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::OTHER_PATIENT_I_DS,
                Vr::Lo,
                Arc::new(crate::model::buffer::MemoryBuffer::from(b"HELLO".to_vec())),
            ),
        ));
        let out = dump_dataset(&ds, &DumpOptions::default());
        assert!(
            out.contains("HELLO"),
            "printable byte strings render as text"
        );

        let mut empty = Dataset::new();
        empty.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::OTHER_PATIENT_I_DS,
                Vr::Lo,
                Arc::new(crate::model::buffer::MemoryBuffer::from(Vec::<u8>::new())),
            ),
        ));
        assert!(dump_dataset(&empty, &DumpOptions::default()).contains("(empty)"));

        let mut fragments = Dataset::new();
        fragments
            .insert_fragments(Tag::FLOAT_PIXEL_DATA, Vr::Ob)
            .push_fragment(Arc::new(crate::model::buffer::MemoryBuffer::from(vec![
                1, 2,
            ])));
        let out = dump_dataset(&fragments, &DumpOptions::default());
        assert!(
            out.contains("fragment(s)"),
            "fragment sequence is rendered: {out}"
        );
    }

    #[test]
    fn a_pixel_summary_reports_the_image_shape() {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::ROWS, 512u16);
        ds.insert_value(Tag::COLUMNS, 256u16);
        ds.insert_value(Tag::BITS_ALLOCATED, 16u16);
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
        ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
        let summary = pixel_summary(&ds).unwrap();
        assert_eq!(summary.rows, 512);
        assert_eq!(summary.columns, 256);
        assert_eq!(summary.bits_allocated, 16);
        assert!(summary.one_line().contains("256x512"));
    }

    #[test]
    fn a_large_on_demand_value_is_previewed_not_materialised() {
        // CORE-09: dumping used to call `data()` on the whole value. A large
        // on-demand window must read only `max_binary_bytes` and report the real
        // size, so the dump stays bounded (the old code rendered all 256 KiB).
        let path = std::env::temp_dir().join(format!(
            "radicom-dump-large-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let payload = vec![0xABu8; 256 * 1024];
        std::fs::write(&path, &payload).unwrap();

        let mut ds = Dataset::new();
        let window =
            crate::model::buffer::FileWindow::open(&path, 0, payload.len() as u64).unwrap();
        ds.insert_or_update(crate::model::element::DicomElement::new_bytes(
            Tag::new(0x0009, 0x1001),
            Vr::Ob,
            Arc::new(window),
        ));

        let options = DumpOptions {
            max_binary_bytes: 8,
            ..DumpOptions::default()
        };
        let out = dump_dataset(&ds, &options);
        std::fs::remove_file(&path).ok();

        assert!(out.contains("AB AB AB AB AB AB AB AB"), "{out}");
        assert!(
            out.contains("(262144 bytes)"),
            "the real size is still reported: {out}"
        );
        assert!(
            out.len() < 4096,
            "the dump must stay bounded, got {} chars",
            out.len()
        );
    }

    #[test]
    fn a_non_image_dataset_has_no_pixel_summary() {
        assert!(pixel_summary(&sample_dataset()).is_none());
    }

    #[test]
    fn dump_previews_file_windows_and_truncates_long_text() {
        let path = std::env::temp_dir().join(format!("radicom-dump-{}.bin", std::process::id()));
        std::fs::write(&path, b"\x01\x02\x03\x04").unwrap();
        let mut ds = Dataset::new();
        let window = crate::model::buffer::FileWindow::open(&path, 0, 4).unwrap();
        ds.insert_or_update(crate::model::element::DicomElement::new_bytes(
            Tag::new(0x0009, 0x1001),
            Vr::Ob,
            Arc::new(window),
        ));
        ds.insert_str(Tag::MODALITY, Vr::Cs, "CT");
        let out = dump_dataset(&ds, &DumpOptions::default());
        assert!(out.contains("01 02 03 04"), "{out}");
        std::fs::remove_file(&path).ok();

        // text longer than max_value_chars is truncated with a count
        let mut long = Dataset::new();
        long.insert_str(Tag::STUDY_DESCRIPTION, Vr::Lo, "abcdefghij");
        let options = DumpOptions {
            max_value_chars: 4,
            ..DumpOptions::default()
        };
        assert!(dump_dataset(&long, &options).contains("… (10 chars)"));
    }

    #[test]
    fn dump_lines_carry_byte_length_uid_names_and_fragment_sizes() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "DOE^JOHN");
        ds.insert_value(Tag::ROWS, 4u16);
        let fragments = ds.insert_fragments(Tag::PIXEL_DATA, Vr::Ob);
        fragments.push_fragment(Arc::new(crate::model::buffer::MemoryBuffer::from(vec![
            0u8;
            4
        ])));
        let out = dump_dataset(&ds, &DumpOptions::default());

        assert!(
            out.contains("(Secondary Capture Image Storage)"),
            "UI values carry the registry name: {out}"
        );
        assert!(
            out.contains("DOE^JOHN # 8"),
            "the element byte length is printed: {out}"
        );
        assert!(out.contains("# 2"), "binary US length is printed: {out}");
        assert!(
            out.contains("(1 fragment(s): 4 bytes)"),
            "per-fragment sizes are printed: {out}"
        );
    }

    #[test]
    fn dump_file_walks_the_file_meta_group_before_the_dataset() {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5");
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
        let path =
            std::env::temp_dir().join(format!("radicom-dump-file-{}.dcm", std::process::id()));
        DicomFile::from_dataset(ds).save(&path).unwrap();
        let file = DicomFile::open(&path).unwrap();
        let out = dump_file(&file, &DumpOptions::default());
        assert!(out.contains("(0002,0010)"), "group 0002 is dumped: {out}");
        let meta = out.find("(0002,").expect("meta line");
        let patient = out.find("(0010,0020)").expect("dataset line");
        assert!(meta < patient, "file meta precedes the dataset: {out}");
        std::fs::remove_file(&path).ok();
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::model::buffer::{ByteBuffer, MemoryBuffer};
    use std::sync::Arc;

    /// Reports a size but fails every range read.
    #[derive(Debug)]
    struct UnreadableBuffer;
    impl ByteBuffer for UnreadableBuffer {
        fn size(&self) -> u64 {
            4
        }
        fn is_memory(&self) -> bool {
            false
        }
        fn data(&self) -> std::io::Result<std::borrow::Cow<'_, [u8]>> {
            Ok(std::borrow::Cow::Borrowed(&[0, 0, 0, 0]))
        }
        fn get_byte_range(&self, _offset: u64, _buf: &mut [u8]) -> std::io::Result<()> {
            Err(std::io::Error::other("boom"))
        }
    }

    #[test]
    fn an_empty_fragment_sequence_dumps_without_sizes() {
        let mut ds = Dataset::new();
        ds.insert_fragments(Tag::PIXEL_DATA, Vr::Ob);
        let out = dump_dataset(&ds, &DumpOptions::default());
        assert!(out.contains("0 fragment(s)"), "{out}");
    }

    #[test]
    fn unreadable_empty_long_and_control_other_values_render() {
        let options = DumpOptions {
            max_binary_bytes: 8,
            max_value_chars: 4,
            ..Default::default()
        };

        // an unreadable on-demand buffer renders as "(unreadable: …)"
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::new(0x7777, 0x0001),
            Vr::Ob,
            Arc::new(UnreadableBuffer),
        )));
        assert!(dump_dataset(&ds, &options).contains("unreadable"));

        // an all-NUL string-typed Other value renders as "(empty)"
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::new(0x7777, 0x0002),
            Vr::Lo,
            Arc::new(MemoryBuffer::from(vec![0u8, 0])),
        )));
        assert!(dump_dataset(&ds, &options).contains("(empty)"));

        // a printable long string-typed Other value is truncated by render_str
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::new(0x7777, 0x0003),
            Vr::Lo,
            Arc::new(MemoryBuffer::from(b"ABCDEFGHIJ".to_vec())),
        )));
        assert!(dump_dataset(&ds, &options).contains('…'));

        // a control-character Other value falls back to the hex preview
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::new(0x7777, 0x0004),
            Vr::Lo,
            Arc::new(MemoryBuffer::from(vec![b'a', 0x01, b'b'])),
        )));
        assert!(dump_dataset(&ds, &options).contains("61 01 62"));
    }

    #[test]
    fn a_byte_backed_uid_has_no_uid_name_lookup() {
        let mut ds = Dataset::new();
        ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
            Tag::SOP_CLASS_UID,
            Vr::Ui,
            Arc::new(MemoryBuffer::from(b"1.2.840.10008.5.1.4.1.1.7".to_vec())),
        )));
        let out = dump_dataset(&ds, &DumpOptions::default());
        assert!(out.contains("1.2.840.10008.5.1.4.1.1.7"), "{out}");
    }
}
