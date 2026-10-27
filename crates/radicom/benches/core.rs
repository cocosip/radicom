//! Dependency-free benchmarks (docs/09-testing.md §8; M8 "benchmarks published").
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
    ds.insert_value(Tag::new(0x0028, 0x0002), 1u16); // SamplesPerPixel
    ds.insert_str(Tag::new(0x0028, 0x0004), Vr::Cs, "MONOCHROME2");
    ds.insert_value(Tag::new(0x0028, 0x0010), 512u16); // Rows
    ds.insert_value(Tag::new(0x0028, 0x0011), 512u16); // Columns
    ds.insert_value(Tag::new(0x0028, 0x0100), 16u16); // BitsAllocated
    ds.insert_value(Tag::new(0x0028, 0x0101), 12u16); // BitsStored
    ds.insert_value(Tag::new(0x0028, 0x0102), 11u16); // HighBit
    ds.insert_value(Tag::new(0x0028, 0x0103), 0u16); // PixelRepresentation
    ds.insert_str(Tag::new(0x0028, 0x0030), Vr::Ds, "0.5\\0.5");
    ds.insert_str(Tag::new(0x0028, 0x1050), Vr::Ds, "2048"); // WindowCenter
    ds.insert_str(Tag::new(0x0028, 0x1051), Vr::Ds, "4096"); // WindowWidth
    // some metadata to give the parser/encoder real work
    for (i, (group, element, vr, value)) in [
        (0x0008u16, 0x0060u16, Vr::Cs, "CT".to_string()),
        (0x0008, 0x0020, Vr::Da, "20260101".to_string()),
        (0x0008, 0x103E, Vr::Lo, "benchmark series".to_string()),
        (0x0010, 0x0010, Vr::Pn, "Doe^John".to_string()),
        (0x0010, 0x0020, Vr::Lo, "PATIENT-0001".to_string()),
        (0x0020, 0x4000, Vr::Lt, "a fairly long comment used for benchmarking".to_string()),
    ]
    .iter()
    .enumerate()
    {
        let _ = i;
        ds.insert_str(Tag::new(*group, *element), *vr, value.clone());
    }
    ds.insert_str(Tag::new(0x0028, 0x0008), Vr::Is, frames.to_string()); // NumberOfFrames

    let pixels = 512 * 512 * frames as usize;
    let mut bytes = Vec::with_capacity(pixels * 2);
    for i in 0..pixels {
        bytes.extend_from_slice(&((i % 4096) as u16).to_le_bytes());
    }
    ds.insert_or_update(radicom::model::item::DicomItem::Element(
        radicom::model::element::DicomElement::new_bytes(
            Tag::new(0x7FE0, 0x0010),
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
    println!("fixture: 512x512x1 16-bit image, {} bytes encoded\n", encoded.len());

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
}
