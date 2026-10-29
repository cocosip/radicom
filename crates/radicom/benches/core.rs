//! Dependency-free benchmarks (docs/02-development-guide.md §5 §8; M8 "benchmarks published").
//!
//! Criterion is not vendored in this workspace's offline cache, so this suite is a
//! plain `harness = false` binary that times the core hot paths with
//! `std::time::Instant` and prints ns/op. Run with `cargo bench -p radicom`
//! (add `--features render` for the frame-render benchmark). It covers the
//! `parse_dataset`, `write_dataset` and `get_frame` rows of the benchmark table;
//! `json_roundtrip` lives in the radicom-json crate and `network_throughput` in
//! radicom-network.

use std::hint::black_box;
use std::time::Instant;

use radicom::io::file::{DicomFile, ReadOptions};
use radicom::io::writer::WriteOptions;
use radicom::model::dataset::Dataset;
use radicom::model::tag::Tag;
use radicom::model::transfer_syntax::syntaxes;
use radicom::model::vr::Vr;
use radicom::pixel::PixelData;
use std::sync::Arc;

/// A `frames`-deep 512×512 16-bit MONOCHROME2 grayscale image with metadata.
fn image_dataset(frames: u32) -> Dataset {
    let mut ds = Dataset::new();
    ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16); // SamplesPerPixel
    ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
    ds.insert_value(Tag::ROWS, 512u16); // Rows
    ds.insert_value(Tag::COLUMNS, 512u16); // Columns
    ds.insert_value(Tag::BITS_ALLOCATED, 16u16); // BitsAllocated
    ds.insert_value(Tag::BITS_STORED, 12u16); // BitsStored
    ds.insert_value(Tag::HIGH_BIT, 11u16); // HighBit
    ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16); // PixelRepresentation
    ds.insert_str(Tag::PIXEL_SPACING, Vr::Ds, "0.5\\0.5");
    ds.insert_str(Tag::WINDOW_CENTER, Vr::Ds, "2048"); // WindowCenter
    ds.insert_str(Tag::WINDOW_WIDTH, Vr::Ds, "4096"); // WindowWidth
                                                      // some metadata to give the parser/encoder real work
    for (i, (group, element, vr, value)) in [
        (0x0008u16, 0x0060u16, Vr::Cs, "CT".to_string()),
        (0x0008, 0x0020, Vr::Da, "20260101".to_string()),
        (0x0008, 0x103E, Vr::Lo, "benchmark series".to_string()),
        (0x0010, 0x0010, Vr::Pn, "Doe^John".to_string()),
        (0x0010, 0x0020, Vr::Lo, "PATIENT-0001".to_string()),
        (
            0x0020,
            0x4000,
            Vr::Lt,
            "a fairly long comment used for benchmarking".to_string(),
        ),
    ]
    .iter()
    .enumerate()
    {
        let _ = i;
        ds.insert_str(Tag::new(*group, *element), *vr, value.clone());
    }
    ds.insert_str(Tag::NUMBER_OF_FRAMES, Vr::Is, frames.to_string()); // NumberOfFrames

    let pixels = 512 * 512 * frames as usize;
    let mut bytes = Vec::with_capacity(pixels * 2);
    for i in 0..pixels {
        bytes.extend_from_slice(&((i % 4096) as u16).to_le_bytes());
    }
    ds.insert_or_update(radicom::model::item::DicomItem::Element(
        radicom::model::element::DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ow,
            Arc::new(radicom::model::buffer::MemoryBuffer::from(bytes)),
        ),
    ));
    ds.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
    ds
}

fn time_ops<F: FnMut()>(iters: u32, mut f: F) -> f64 {
    // warm up so the first allocation/page-fault isn't counted
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
    println!("{name:<22} {ns_per_op:>12.1} ns/op   ({iters} iters)");
}

fn main() {
    println!("radicom benchmarks (std harness, not criterion)\n");

    // prepare serialized bytes for parse / get_frame benches
    let dataset = image_dataset(1);
    let file = DicomFile::from_dataset(dataset.clone());
    let encoded = file.to_bytes(&WriteOptions::default()).expect("serialize");
    println!(
        "fixture: 512x512x1 16-bit image, {} bytes encoded\n",
        encoded.len()
    );

    // parse_dataset
    let iters = 2_000;
    let ns = time_ops(iters, || {
        let parsed = DicomFile::from_bytes(black_box(&encoded), &ReadOptions::default()).unwrap();
        black_box(parsed);
    });
    report("parse_dataset", ns, iters);

    // write_dataset
    let iters = 2_000;
    let ns = time_ops(iters, || {
        let out = file.to_bytes(&WriteOptions::default()).unwrap();
        black_box(out);
    });
    report("write_dataset", ns, iters);

    // get_frame (uncompressed slicing)
    let iters = 20_000;
    let ns = time_ops(iters, || {
        let mut ds = dataset.clone();
        let pixel = PixelData::from_dataset(&mut ds).unwrap();
        let frame = pixel.get_frame(0).unwrap();
        black_box(frame);
    });
    report("get_frame", ns, iters);

    // render_frame (feature `render`): 16-bit CT frame → BGRA32
    #[cfg(feature = "render")]
    {
        let ds = image_dataset(1);
        let iters = 500;
        let ns = time_ops(iters, || {
            let img = radicom::render::render_frame(black_box(&ds), 0).unwrap();
            black_box(img);
        });
        report("render_frame", ns, iters);
    }

    // file vs memory parsing of a many-element dataset: the file path must not
    // pay per-element syscalls
    let many = many_element_dataset(800);
    let many_file = DicomFile::from_dataset(many);
    let many_bytes = many_file.to_bytes(&WriteOptions::default()).unwrap();
    let path = std::env::temp_dir().join(format!("radicom-bench-{}.dcm", std::process::id()));
    std::fs::write(&path, &many_bytes).expect("write bench fixture");
    let iters = 300;
    let ns = time_ops(iters, || {
        let opened = DicomFile::open(black_box(&path)).unwrap();
        black_box(opened);
    });
    report("open_800_elements", ns, iters);
    let ns = time_ops(iters, || {
        let parsed =
            DicomFile::from_bytes(black_box(&many_bytes), &ReadOptions::default()).unwrap();
        black_box(parsed);
    });
    report("from_bytes_800", ns, iters);
    let _ = std::fs::remove_file(&path);
}

/// 800 small elements: the shape that exposes per-element parsing overhead.
fn many_element_dataset(count: u16) -> Dataset {
    let mut ds = Dataset::new();
    for i in 0..count {
        // an even, unassigned group: 800 distinct tags with no private creator
        // bookkeeping in the way
        ds.insert_str(Tag::new(0x7777, 0x0010 + i), Vr::Lo, format!("value-{i}"));
    }
    ds.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
    ds
}
