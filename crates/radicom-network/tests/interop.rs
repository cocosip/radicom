//! Network self-interop (SCU ↔ SCP, hermetic) + optional external-peer interop.
//!
//! The `self_interop_*` tests spin a local `DicomServer` and drive it with
//! `DicomClient` over real TCP on 127.0.0.1 — no external peer required, so
//! radicom's own network stack is exercised end to end in CI.
//!
//! `external_peer_*` is `#[ignore]`d: it needs `RADICOM_PEER` pointing at a real
//! DICOM peer, so a plain `cargo test` reports it as ignored rather than a
//! vacuous pass (docs/04-interop.md).

use async_trait::async_trait;
use radicom::io::file::DicomFile;
use radicom::model::tag::Tag;
use radicom_network::association::sop_class;
use radicom_network::client::DicomClient;
use radicom_network::server::{DicomServer, ServerOptions};
use radicom_network::services::{CEchoRequest, CEchoResponse, CStoreRequest, CStoreResponse, ServiceProvider};
use radicom_network::store::StoreSink;
use std::sync::{Arc, Mutex};

/// A minimal SCP provider that records every C-STORE instance it receives.
struct CaptureProvider {
    sops: Vec<String>,
    files: Mutex<Vec<DicomFile>>,
}

impl CaptureProvider {
    fn new(sops: Vec<String>) -> Self {
        CaptureProvider { sops, files: Mutex::new(Vec::new()) }
    }
}

#[async_trait]
impl ServiceProvider for CaptureProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        self.sops.clone()
    }
    async fn on_c_echo(&self, request: &CEchoRequest) -> radicom_network::Result<CEchoResponse> {
        Ok(CEchoResponse::success(request))
    }
    async fn on_c_store(&self, request: &CStoreRequest) -> radicom_network::Result<CStoreResponse> {
        self.files.lock().unwrap().push(request.file.clone());
        Ok(CStoreResponse::success(request))
    }
}

fn corpus_file(rel: &str) -> DicomFile {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests-data").join(rel);
    DicomFile::open(&path).unwrap_or_else(|e| panic!("open {}: {e:?}", path.display()))
}

#[tokio::test]
async fn self_interop_c_echo_over_tcp() {
    let provider = Arc::new(CaptureProvider::new(vec![sop_class::VERIFICATION.to_string()]));
    let server = DicomServer::bind("127.0.0.1:0", provider.clone(), ServerOptions::default())
        .await
        .expect("bind");
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
    let echo = client.send_c_echo(sop_class::VERIFICATION).await.expect("C-ECHO");
    assert_eq!(echo.status, 0x0000, "C-ECHO accepted");
    client.release().await;
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn self_interop_c_store_real_file_round_trips() {
    // A real fo-dicom-sourced file, sent over the wire and captured by the SCP.
    let file = corpus_file("explicit-le/GH178.dcm");
    let sop = file
        .dataset
        .try_get_str(Tag::SOP_CLASS_UID)
        .expect("SOP class UID")
        .trim()
        .to_string();
    let provider = Arc::new(CaptureProvider::new(vec![sop_class::VERIFICATION.to_string(), sop]));
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions { store_sink: StoreSink::Memory, ..Default::default() },
    )
    .await
    .expect("bind");
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
    let response = client.send_c_store(&file).await.expect("C-STORE");
    assert_eq!(response.status, 0x0000, "C-STORE accepted");
    client.release().await;
    handle.await.unwrap().unwrap();

    let stored = provider.files.lock().unwrap();
    assert_eq!(stored.len(), 1, "the SCP captured exactly one instance");

    // §4 semantic tolerance: the writer recomputes/strips redundant (gggg,0000)
    // group-length elements, so strip them from both sides before comparing.
    let mut original = file.dataset.clone();
    let mut received = stored[0].dataset.clone();
    strip_group_lengths(&mut original);
    strip_group_lengths(&mut received);
    assert!(received == original, "wire round-trip preserves every element (group lengths normalised)");
    assert_eq!(
        stored[0].dataset.transfer_syntax().uid(),
        file.dataset.transfer_syntax().uid(),
        "transfer syntax is preserved across the wire"
    );
}

/// Removes (gggg,0000) group-length elements recursively (they are derived).
fn strip_group_lengths(dataset: &mut radicom::model::dataset::Dataset) {
    use radicom::model::item::DicomItem;
    let group_length_tags: Vec<_> = dataset
        .items()
        .filter(|item| item.tag().element() == 0x0000)
        .map(|item| item.tag().clone())
        .collect();
    for tag in group_length_tags {
        dataset.remove(&tag);
    }
    let sequence_tags: Vec<_> = dataset
        .items()
        .filter_map(|item| match item {
            DicomItem::Sequence(sequence) => Some(sequence.tag().clone()),
            _ => None,
        })
        .collect();
    for tag in sequence_tags {
        if let Some(DicomItem::Sequence(mut sequence)) = dataset.remove(&tag) {
            for item in sequence.items_mut() {
                strip_group_lengths(item);
            }
            dataset.insert_or_update(DicomItem::Sequence(sequence));
        }
    }
}

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

#[tokio::test]
#[ignore = "needs RADICOM_PEER=host:port pointing at a real DICOM peer; run with --ignored"]
async fn external_peer_c_echo_and_optional_c_store() {
    let Ok(peer) = std::env::var("RADICOM_PEER") else {
        eprintln!("skip: set RADICOM_PEER=host:port to run interop against a real peer");
        return;
    };
    let calling = setting("RADICOM_CALLING", "RADICOM");
    let called = setting("RADICOM_CALLED", "ANY-SCP");

    let mut client = DicomClient::new(calling.clone(), called.clone(), peer.clone());
    let echo = client.send_c_echo(sop_class::VERIFICATION).await.expect("C-ECHO failed");
    assert_eq!(echo.status, 0x0000, "peer {peer} rejected C-ECHO: 0x{:04X}", echo.status);
    client.release().await;

    if let Ok(path) = std::env::var("RADICOM_STORE_FILE") {
        let file = radicom::io::file::DicomFile::open(&path).expect("open store file");
        let mut client = DicomClient::new(calling, called, peer.clone());
        let resp = client.send_c_store(&file).await.expect("C-STORE failed");
        assert_eq!(resp.status, 0x0000, "peer {peer} failed C-STORE: 0x{:04X}", resp.status);
        client.release().await;
    }
}
