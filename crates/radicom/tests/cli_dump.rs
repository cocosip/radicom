//! Integration test for the INF-07 dump CLI building blocks (dump + pixel summary).
//!
//! Exercises the same library path `radicom-dump` uses: write a file, read it
//! back, dump its dataset and summarise its pixels.

use radicom::dump::{dump_dataset, pixel_summary, DumpOptions};
use radicom::io::file::DicomFile;
use radicom::model::dataset::Dataset;
use radicom::model::tag::Tag;
use radicom::model::vr::Vr;

fn write_sample(path: &std::path::Path) {
    let mut ds = Dataset::new();
    ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "DOE^JANE");
    ds.insert_str(Tag::MODALITY, Vr::Cs, "CT");
    ds.insert_value(Tag::ROWS, 4u16);
    ds.insert_value(Tag::COLUMNS, 4u16);
    ds.insert_value(Tag::BITS_ALLOCATED, 16u16);
    ds.insert_value(Tag::BITS_STORED, 16u16);
    ds.insert_value(Tag::HIGH_BIT, 15u16);
    ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
    ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
    ds.insert_or_update(radicom::model::item::DicomItem::Element(
        radicom::model::element::DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            std::sync::Arc::new(radicom::model::buffer::MemoryBuffer::from(vec![0u8; 4 * 4 * 2])),
        ),
    ));
    DicomFile::from_dataset(ds).save(path).unwrap();
}

#[test]
fn dumping_a_written_file_shows_header_and_pixels() {
    let path = std::env::temp_dir().join(format!("radicom-dump-it-{}.dcm", std::process::id()));
    write_sample(&path);

    // the CLI's flow: open, print syntax, summary, dump
    let file = DicomFile::open(&path).unwrap();
    let syntax = file.dataset.transfer_syntax().uid().to_string();
    assert!(!syntax.is_empty(), "a transfer syntax is reported");

    let summary = pixel_summary(&file.dataset).expect("an image dataset has a pixel summary");
    assert_eq!(summary.rows, 4);
    assert_eq!(summary.columns, 4);
    assert!(summary.one_line().contains("MONOCHROME2"));

    let dump = dump_dataset(&file.dataset, &DumpOptions::default());
    assert!(dump.contains("PatientName"));
    assert!(dump.contains("DOE^JANE"));
    assert!(dump.contains("(0028,0010)"), "shows image geometry tags");

    let _ = std::fs::remove_file(path);
}
