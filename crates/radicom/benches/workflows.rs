//! Workflow benchmarks beyond JSON/XML (docs/02-development-guide.md §5.8).
//!
//! Criterion is not vendored in this workspace's offline cache, so this is a
//! dependency-free `harness = false` binary timing the non-serialization hot
//! paths: character-set encode/decode, deflate, transcoding, validation,
//! DICOMDIR building/saving, directory scanning and on-demand large-value
//! opening. Run with:
//!
//! ```text
//! cargo bench -p radicom --all-features
//! ```
//!
//! `anonymize` only runs with the `anonymize` feature (included in
//! `--all-features`).

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use radicom::io::file::{DicomFile, FileReadOption, ReadOptions};
use radicom::io::writer::WriteOptions;
use radicom::media::{scan_directory, DicomDirectory, ScanOptions};
use radicom::model::dataset::Dataset;
use radicom::model::element::DicomElement;
use radicom::model::item::DicomItem;
use radicom::model::tag::Tag;
use radicom::model::transfer_syntax::syntaxes;
use radicom::model::vr::Vr;
use radicom::pixel::transcoder::Transcoder;

fn time_ops<F: FnMut()>(iters: u32, mut f: F) -> f64 {
    for _ in 0..(iters / 10).max(1) {
        f();
    }
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed().as_nanos() as f64 / iters as f64
}

fn report(name: &str, ns_per_op: f64, iters: u32) {
    println!("{name:<26} {ns_per_op:>12.1} ns/op   ({iters} iters)");
}

fn report_throughput(name: &str, ns_per_op: f64, bytes: u64, iters: u32) {
    let mib_s = (bytes as f64 / (1024.0 * 1024.0)) / (ns_per_op / 1e9);
    println!("{name:<26} {ns_per_op:>12.1} ns/op   {mib_s:>8.1} MiB/s ({iters} iters)");
}

/// A 512×512 16-bit MONOCHROME2 image with the metadata needed to write it.
fn image_dataset(frames: u32) -> Dataset {
    let mut ds = Dataset::new();
    ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
    ds.insert_str(
        Tag::SOP_INSTANCE_UID,
        Vr::Ui,
        "1.2.826.0.1.3680043.10.1234.2",
    );
    ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
    ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
    ds.insert_value(Tag::ROWS, 512u16);
    ds.insert_value(Tag::COLUMNS, 512u16);
    ds.insert_value(Tag::BITS_ALLOCATED, 16u16);
    ds.insert_value(Tag::BITS_STORED, 12u16);
    ds.insert_value(Tag::HIGH_BIT, 11u16);
    ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
    ds.insert_str(Tag::NUMBER_OF_FRAMES, Vr::Is, frames.to_string());
    let pixels = 512 * 512 * frames as usize;
    let mut bytes = Vec::with_capacity(pixels * 2);
    for i in 0..pixels {
        bytes.extend_from_slice(&((i % 4096) as u16).to_le_bytes());
    }
    ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
        Tag::PIXEL_DATA,
        Vr::Ow,
        Arc::new(radicom::model::buffer::MemoryBuffer::from(bytes)),
    )));
    ds.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
    ds
}

fn text_dataset(elements: u16) -> Dataset {
    let mut ds = Dataset::new();
    ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
    for i in 0..elements {
        ds.insert_str(
            Tag::new(0x7777, 0x0010 + i),
            Vr::Lo,
            format!("workflow value {i}"),
        );
    }
    ds
}

fn main() {
    println!("radicom workflow benchmarks (std harness, not criterion)\n");

    charset_utf8();
    charset_iso2022();
    deflate();
    transcode();
    validate();
    dicomdir();
    scan();
    on_demand_open();
    #[cfg(feature = "anonymize")]
    anonymize();
}

fn charset_utf8() {
    const NAME: &str = "张^小东\\李^四\\王^五\\赵^六";
    let mut ds = Dataset::new();
    ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
    ds.insert_str(
        Tag::SOP_INSTANCE_UID,
        Vr::Ui,
        "1.2.826.0.1.3680043.10.1234.3",
    );
    ds.insert_str(Tag::SPECIFIC_CHARACTER_SET, Vr::Cs, "ISO_IR 192");
    ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, NAME);
    let file = DicomFile::from_dataset(ds);
    let bytes = file
        .to_bytes(&WriteOptions::default())
        .expect("utf-8 encode");
    let iters = 20_000;
    let ns = time_ops(iters, || {
        black_box(file.to_bytes(black_box(&WriteOptions::default())).unwrap());
    });
    report("charset_utf8_write", ns, iters);
    let ns = time_ops(iters, || {
        let parsed = DicomFile::from_bytes(black_box(&bytes), &ReadOptions::default()).unwrap();
        black_box(&parsed);
        let _ = black_box(parsed.dataset.get_str(Tag::PATIENT_NAME));
    });
    report("charset_utf8_read", ns, iters);
}

fn charset_iso2022() {
    // A declared ISO 2022 charset must carry its designator escapes; this
    // exercises the encoder path instead of the plain UTF-8 one.
    let mut ds = Dataset::new();
    ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
    ds.insert_str(
        Tag::SOP_INSTANCE_UID,
        Vr::Ui,
        "1.2.826.0.1.3680043.10.1234.4",
    );
    ds.insert_str(Tag::SPECIFIC_CHARACTER_SET, Vr::Cs, "\\ISO 2022 IR 58");
    ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "张^小东");
    let file = DicomFile::from_dataset(ds);
    let bytes = file
        .to_bytes(&WriteOptions::default())
        .expect("iso 2022 encode");
    let iters = 20_000;
    let ns = time_ops(iters, || {
        black_box(file.to_bytes(black_box(&WriteOptions::default())).unwrap());
    });
    report("charset_iso2022_write", ns, iters);
    let ns = time_ops(iters, || {
        let parsed = DicomFile::from_bytes(black_box(&bytes), &ReadOptions::default()).unwrap();
        black_box(&parsed);
        let _ = black_box(parsed.dataset.get_str(Tag::PATIENT_NAME));
    });
    report("charset_iso2022_read", ns, iters);
}

fn deflate() {
    let mut ds = image_dataset(1);
    ds.set_transfer_syntax(syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone());
    let file = DicomFile::from_dataset(ds);
    let bytes = file.to_bytes(&WriteOptions::default()).expect("deflate");
    let plain_bytes = file
        .dataset
        .get::<radicom::model::buffer::SharedBuffer>(Tag::PIXEL_DATA)
        .expect("pixel data")
        .size();
    let iters = 20;
    let ns = time_ops(iters, || {
        black_box(file.to_bytes(black_box(&WriteOptions::default())).unwrap());
    });
    report_throughput("deflate_write", ns, plain_bytes, iters);
    let ns = time_ops(iters, || {
        black_box(DicomFile::from_bytes(black_box(&bytes), &ReadOptions::default()).unwrap());
    });
    report_throughput("deflate_read", ns, plain_bytes, iters);
}

fn transcode() {
    let ds = image_dataset(1);
    let transcoder = Transcoder::new(
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
        syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(),
        None,
        None,
    );
    let iters = 200;
    let ns = time_ops(iters, || {
        black_box(transcoder.transcode_dataset(black_box(&ds)).unwrap());
    });
    report("transcode_uncompressed", ns, iters);
}

fn validate() {
    let ds = text_dataset(500);
    let iters = 2_000;
    let ns = time_ops(iters, || {
        ds.validate().expect("valid dataset");
        black_box(());
    });
    report("validate_500_elements", ns, iters);
}

fn dicomdir() {
    let files: Vec<DicomFile> = (0..100)
        .map(|i| {
            let mut ds = Dataset::new();
            ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.2");
            ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, format!("1.2.3.4.5.6.7.{i}"));
            ds.insert_str(Tag::PATIENT_ID, Vr::Lo, format!("PID-{i}"));
            ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, format!("DOE^JOHN{i}"));
            ds.insert_str(
                Tag::STUDY_INSTANCE_UID,
                Vr::Ui,
                format!("1.2.3.4.5.6.8.{i}"),
            );
            ds.insert_str(
                Tag::SERIES_INSTANCE_UID,
                Vr::Ui,
                format!("1.2.3.4.5.6.9.{i}"),
            );
            DicomFile::from_dataset(ds)
        })
        .collect();

    let iters = 20;
    let ns = time_ops(iters, || {
        let mut dir = DicomDirectory::new("RADICOM^BENCH");
        for (i, file) in files.iter().enumerate() {
            dir.add_file(file, format!("IMAGES/IM-{i}.dcm"))
                .expect("add to DICOMDIR");
        }
        black_box(dir);
    });
    report("dicomdir_add_100", ns, iters);

    let path = std::env::temp_dir().join(format!("radicom-bench-dicomdir-{}", std::process::id()));
    let mut dir = DicomDirectory::new("RADICOM^BENCH");
    for (i, file) in files.iter().enumerate() {
        dir.add_file(file, format!("IMAGES/IM-{i}.dcm")).unwrap();
    }
    let iters = 10;
    let ns = time_ops(iters, || {
        let mut dir = dir.clone();
        dir.save(black_box(&path)).unwrap();
    });
    report("dicomdir_save_100", ns, iters);
    let _ = std::fs::remove_file(&path);
}

fn scan() {
    let dir = std::env::temp_dir().join(format!("radicom-bench-scan-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scan fixture dir");
    let mut ds = Dataset::new();
    ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.2");
    ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5.6.7");
    ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "DOE^SCAN");
    let file = DicomFile::from_dataset(ds);
    for i in 0..200 {
        let path = dir.join(format!("IM-{i}.dcm"));
        file.save(&path).expect("write scan fixture");
    }
    let options = ScanOptions {
        glob: Some("*.dcm".to_string()),
        ..Default::default()
    };
    let iters = 5;
    let ns = time_ops(iters, || {
        let mut visitor = |_event: radicom::media::ScanEvent| {};
        let summary = scan_directory(black_box(&dir), &options, &mut visitor, |path| {
            let _ = DicomFile::open(path).map(|file| black_box(file.dataset.len()));
            Ok(())
        })
        .expect("scan");
        black_box(summary);
    });
    report("scan_200_files", ns, iters);
    let _ = std::fs::remove_dir_all(&dir);
}

fn on_demand_open() {
    let ds = image_dataset(32); // 512*512*2*32 = 16 MiB of pixel data
    let path = std::env::temp_dir().join(format!("radicom-bench-large-{}", std::process::id()));
    DicomFile::from_dataset(ds)
        .save(&path)
        .expect("write large fixture");
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    let on_demand = ReadOptions {
        read: FileReadOption::ReadLargeOnDemand,
        large_object_size: 64 * 1024,
    };
    let iters = 100;
    let ns = time_ops(iters, || {
        black_box(DicomFile::open_with_options(black_box(&path), &on_demand).unwrap());
    });
    report_throughput("open_on_demand_16MiB", ns, bytes, iters);

    let read_all = ReadOptions {
        read: FileReadOption::ReadAll,
        large_object_size: 64 * 1024,
    };
    let iters = 20;
    let ns = time_ops(iters, || {
        black_box(DicomFile::open_with_options(black_box(&path), &read_all).unwrap());
    });
    report_throughput("open_read_all_16MiB", ns, bytes, iters);
    let _ = std::fs::remove_file(&path);
}

#[cfg(feature = "anonymize")]
fn anonymize() {
    use radicom::anonymizer::Anonymizer;

    let mut ds = text_dataset(100);
    ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-12345");
    ds.insert_str(
        Tag::STUDY_INSTANCE_UID,
        Vr::Ui,
        "1.2.840.10008.5.1.4.1.1.2.99",
    );
    ds.insert_str(Tag::ACCESSION_NUMBER, Vr::Sh, "ACC-42");
    ds.insert_str(Tag::STUDY_DATE, Vr::Da, "20260101");
    let anon = Anonymizer::basic();
    let iters = 2_000;
    let ns = time_ops(iters, || {
        black_box(anon.anonymize(black_box(&ds)).unwrap());
    });
    report("anonymize_100_elements", ns, iters);
}
