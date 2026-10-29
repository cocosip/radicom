//! Dependency-free network benchmark (docs/02-development-guide.md §5 §8: `network_throughput`).
//!
//! Criterion is not vendored offline, so this is a `harness = false` binary that
//! drives a loopback C-STORE of a multi-frame file against an in-process SCP and
//! reports throughput in MB/s. Run with `cargo bench -p radicom-network`.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use radicom::io::file::DicomFile;
use radicom::model::dataset::Dataset;
use radicom::model::tag::Tag;
use radicom::model::transfer_syntax::syntaxes;
use radicom::model::vr::Vr;
use radicom::pixel::PixelData;
use radicom_network::server::{DicomServer, ServerOptions};
use radicom_network::services::{CStoreRequest, CStoreResponse, ServiceProvider};
use radicom_network::store::StoreSink;
use radicom_network::DicomClient;

const SOP_CLASS: &str = "1.2.840.10008.5.1.4.1.1.7"; // Secondary Capture

struct StoreProvider;

#[async_trait::async_trait]
impl ServiceProvider for StoreProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![SOP_CLASS.to_string()]
    }
    async fn on_c_store(&self, request: &CStoreRequest) -> radicom_network::Result<CStoreResponse> {
        Ok(CStoreResponse::success(request))
    }
}

/// A `frames`-deep 256×256 16-bit MONOCHROME2 secondary-capture instance.
fn multiframe_file(frames: u32) -> (DicomFile, u64) {
    let (rows, cols) = (256u16, 256u16);
    let mut ds = Dataset::new();
    ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, SOP_CLASS);
    ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5.6.7.8.9");
    ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16); // SamplesPerPixel
    ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
    ds.insert_value(Tag::ROWS, rows);
    ds.insert_value(Tag::COLUMNS, cols);
    ds.insert_value(Tag::BITS_ALLOCATED, 16u16); // BitsAllocated
    ds.insert_value(Tag::BITS_STORED, 16u16); // BitsStored
    ds.insert_value(Tag::HIGH_BIT, 15u16); // HighBit
    ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16); // PixelRepresentation
    ds.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());

    let frame_bytes = (rows as usize) * (cols as usize) * 2;
    {
        let mut pixel = PixelData::create_new(&mut ds).expect("create pixel data");
        let data = vec![0u8; frame_bytes];
        for _ in 0..frames {
            pixel.add_frame(&data).expect("add frame");
        }
    }
    (DicomFile::from_dataset(ds), (frame_bytes * frames as usize) as u64)
}

#[tokio::main]
async fn main() {
    println!("radicom-network benchmarks (std harness, not criterion)\n");

    let frames = 10u32;
    let (file, per_store_bytes) = multiframe_file(frames);
    println!(
        "fixture: {frames} x 256x256 16-bit frames, {:.2} MiB per C-STORE\n",
        per_store_bytes as f64 / (1024.0 * 1024.0)
    );

    // Two rows: the shipped default (`Memory`) and the on-disk `SpoolToTemp`
    // alternative, so the reported numbers include the production default
    // rather than only the fast non-default path (§4.9).
    run_case("default_memory", StoreSink::default(), &file, per_store_bytes).await;
    run_case("spool_to_temp", StoreSink::SpoolToTemp, &file, per_store_bytes).await;
}

async fn run_case(label: &str, sink: StoreSink, file: &DicomFile, per_store_bytes: u64) {
    let server = DicomServer::bind(
        "127.0.0.1:0",
        Arc::new(StoreProvider),
        ServerOptions { store_sink: sink, ..Default::default() },
    )
    .await
    .expect("bind server");
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
    // warm up (also opens the association)
    client.send_c_store(file).await.expect("warmup store");

    let iters = 50u32;
    let start = Instant::now();
    for _ in 0..iters {
        let response = client.send_c_store(black_box(file)).await.expect("store");
        black_box(response);
    }
    let elapsed = start.elapsed();

    let total = per_store_bytes * iters as u64;
    let mb_per_s = (total as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
    println!(
        "network_throughput/{label:<16} {mb_per_s:>8.1} MiB/s   ({iters} stores, {:.1} MiB in {:.3}s)",
        total as f64 / (1024.0 * 1024.0),
        elapsed.as_secs_f64()
    );

    client.release().await;
    handle.await.expect("join server").expect("server ok");
}
