//! Dependency-free JSON benchmarks (docs/02-development-guide.md §5.8: `json_roundtrip`).
//!
//! Criterion is not vendored offline, so this is a `harness = false` binary that
//! times dataset → JSON and dataset → JSON → dataset with std::time::Instant.
//! Run with `cargo bench -p radicom-json`.

use std::hint::black_box;
use std::time::Instant;

use radicom::model::dataset::Dataset;
use radicom::model::tag::Tag;
use radicom::model::vr::Vr;
use radicom_json::{dataset_to_json, json_to_dataset, JsonOptions};

/// A dataset with a spread of VRs so encode/decode do real work.
fn sample() -> Dataset {
    let mut ds = Dataset::new();
    let elements: [(u16, u16, Vr, &str); 16] = [
        (0x0008, 0x0020, Vr::Da, "20260101"),
        (0x0008, 0x0030, Vr::Tm, "120000"),
        (0x0008, 0x0060, Vr::Cs, "CT"),
        (0x0008, 0x103E, Vr::Lo, "benchmark series"),
        (0x0010, 0x0010, Vr::Pn, "Doe^John"),
        (0x0010, 0x0020, Vr::Lo, "PATIENT-0001"),
        (0x0010, 0x0030, Vr::Da, "19800101"),
        (0x0010, 0x0040, Vr::Cs, "M"),
        (0x0020, 0x000D, Vr::Ui, "1.2.840.10008.1.2.1"),
        (0x0020, 0x000E, Vr::Ui, "1.2.3.4.5.6.7.8.9"),
        (0x0020, 0x0011, Vr::Is, "7"),
        (0x0020, 0x0013, Vr::Is, "12"),
        (
            0x0020,
            0x4000,
            Vr::Lt,
            "a fairly long comment used for benchmarking the JSON path",
        ),
        (0x0028, 0x0010, Vr::Us, "512"),
        (0x0028, 0x0011, Vr::Us, "512"),
        (0x0028, 0x1050, Vr::Ds, "2048"),
    ];
    for (group, element, vr, value) in elements {
        ds.insert_str(Tag::new(group, element), vr, value.to_string());
    }
    ds
}

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
    println!("{name:<22} {ns_per_op:>12.1} ns/op   ({iters} iters)");
}

fn main() {
    println!("radicom-json benchmarks (std harness, not criterion)\n");
    let dataset = sample();
    let options = JsonOptions::default();
    let json = dataset_to_json(&dataset, &options).expect("encode");
    println!(
        "fixture: {} elements, {} bytes of JSON\n",
        dataset.len(),
        json.len()
    );

    let iters = 20_000;
    let ns = time_ops(iters, || {
        black_box(dataset_to_json(black_box(&dataset), &options).unwrap());
    });
    report("dataset_to_json", ns, iters);

    let iters = 10_000;
    let ns = time_ops(iters, || {
        let back = json_to_dataset(black_box(&json), &options).unwrap();
        black_box(back);
    });
    report("json_to_dataset", ns, iters);

    let iters = 8_000;
    let ns = time_ops(iters, || {
        let text = dataset_to_json(black_box(&dataset), &options).unwrap();
        let back = json_to_dataset(&text, &options).unwrap();
        black_box(back);
    });
    report("json_roundtrip", ns, iters);
}
