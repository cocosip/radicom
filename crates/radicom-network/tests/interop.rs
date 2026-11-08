//! Network self-interop (SCU ↔ SCP, hermetic) + optional external-peer interop.
//!
//! The `self_interop_*` tests spin a local `DicomServer` and drive it with
//! `DicomClient` over real TCP on 127.0.0.1 — no external peer required, so
//! radicom's own network stack is exercised end to end in CI.
//!
//! `external_peer_*` is `#[ignore]`d: it needs `RADICOM_PEER` pointing at a real
//! DICOM peer, so a plain `cargo test` reports it as ignored rather than a
//! vacuous pass (docs/02-development-guide.md §4).

use async_trait::async_trait;
use radicom::io::file::DicomFile;
use radicom::model::dataset::Dataset;
use radicom::model::tag::Tag;
use radicom::model::transfer_syntax::syntaxes;
use radicom::model::vr::Vr;
use radicom_network::association::sop_class;
use radicom_network::cfind::{CFindRequest, CFindResponse};
use radicom_network::cget::{CGetEvent, CGetRequest};
use radicom_network::client::DicomClient;
use radicom_network::cmove::{CMoveRequest, CMoveResponse};
use radicom_network::server::{ClientOptions, DicomServer, ServerOptions};
use radicom_network::services::{
    CEchoRequest, CEchoResponse, CStoreRequest, CStoreResponse, ServiceProvider,
};
use radicom_network::store::StoreSink;
use std::sync::{Arc, Mutex};

/// A minimal SCP provider that records every C-STORE instance it receives.
struct CaptureProvider {
    sops: Vec<String>,
    files: Mutex<Vec<DicomFile>>,
}

impl CaptureProvider {
    fn new(sops: Vec<String>) -> Self {
        CaptureProvider {
            sops,
            files: Mutex::new(Vec::new()),
        }
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
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests-data")
        .join(rel);
    DicomFile::open(&path).unwrap_or_else(|e| panic!("open {}: {e:?}", path.display()))
}

#[tokio::test]
async fn self_interop_c_echo_over_tcp() {
    let provider = Arc::new(CaptureProvider::new(vec![
        sop_class::VERIFICATION.to_string()
    ]));
    let server = DicomServer::bind("127.0.0.1:0", provider.clone(), ServerOptions::default())
        .await
        .expect("bind");
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
    let echo = client
        .send_c_echo(sop_class::VERIFICATION)
        .await
        .expect("C-ECHO");
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
    let provider = Arc::new(CaptureProvider::new(vec![
        sop_class::VERIFICATION.to_string(),
        sop,
    ]));
    let server = DicomServer::bind(
        "127.0.0.1:0",
        provider.clone(),
        ServerOptions {
            store_sink: StoreSink::Memory,
            ..Default::default()
        },
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

    // §11.2 semantic tolerance: the writer recomputes/strips redundant (gggg,0000)
    // group-length elements, so strip them from both sides before comparing.
    let mut original = file.dataset.clone();
    let mut received = stored[0].dataset.clone();
    strip_group_lengths(&mut original);
    strip_group_lengths(&mut received);
    assert!(
        received == original,
        "wire round-trip preserves every element (group lengths normalised)"
    );
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
    let echo = client
        .send_c_echo(sop_class::VERIFICATION)
        .await
        .expect("C-ECHO failed");
    assert_eq!(
        echo.status, 0x0000,
        "peer {peer} rejected C-ECHO: 0x{:04X}",
        echo.status
    );
    client.release().await;

    if let Ok(path) = std::env::var("RADICOM_STORE_FILE") {
        let file = radicom::io::file::DicomFile::open(&path).expect("open store file");
        let mut client = DicomClient::new(calling, called, peer.clone());
        let resp = client.send_c_store(&file).await.expect("C-STORE failed");
        assert_eq!(
            resp.status, 0x0000,
            "peer {peer} failed C-STORE: 0x{:04X}",
            resp.status
        );
        client.release().await;
    }
}

// ---------------------------------------------------------------------------
// Query/Retrieve self-interop (SCU <-> SCP over real TCP, no external peer).
//
// The three tests below complete the C-series coverage in this integration
// suite: `self_interop_c_echo_over_tcp` and `self_interop_c_store_real_file_*`
// already exercise C-ECHO and C-STORE, so C-FIND / C-GET / C-MOVE are the
// remaining upper-layer commands that need a real client+server pair.
// ---------------------------------------------------------------------------

fn instance(uid: &str) -> DicomFile {
    let mut dataset = Dataset::new();
    dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
    dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, uid);
    dataset.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
    dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
    DicomFile::from_dataset(dataset)
}

/// A valid uncompressed 8-bit instance of `rows * columns` bytes, used to drive
/// the receive spool with an instance far larger than one P-DATA PDU (N4).
fn large_instance(uid: &str, rows: u16, columns: u16) -> DicomFile {
    let mut dataset = Dataset::new();
    dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
    dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, uid);
    dataset.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
    dataset.insert_value(Tag::ROWS, rows);
    dataset.insert_value(Tag::COLUMNS, columns);
    dataset.insert_value(Tag::BITS_ALLOCATED, 8u16);
    dataset.insert_value(Tag::BITS_STORED, 8u16);
    dataset.insert_value(Tag::HIGH_BIT, 7u16);
    dataset.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
    dataset.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
    dataset.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
    {
        let mut pixel = radicom::pixel::PixelData::create_new(&mut dataset).unwrap();
        pixel
            .add_frame(vec![0x5Au8; rows as usize * columns as usize])
            .unwrap();
    }
    dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
    DicomFile::from_dataset(dataset)
}

/// An SCP that answers a C-FIND with one Pending item per match, then Success.
struct FindProvider {
    matches: Vec<String>,
}

#[async_trait]
impl ServiceProvider for FindProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![sop_class::STUDY_ROOT_QR_FIND.to_string()]
    }
    fn on_c_find<'a>(
        &'a self,
        request: &'a CFindRequest,
    ) -> futures::stream::BoxStream<'a, radicom_network::Result<CFindResponse>> {
        let total = self.matches.len() as u16;
        let mut responses = Vec::with_capacity(self.matches.len() + 1);
        for uid in &self.matches {
            let mut identifier = Dataset::new();
            identifier.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, uid.clone());
            identifier.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
            responses.push(Ok(CFindResponse::pending(request, identifier)));
        }
        responses.push(Ok(CFindResponse::success(request, total)));
        Box::pin(futures::stream::iter(responses))
    }
}

#[tokio::test]
async fn self_interop_c_find_returns_every_match_over_tcp() {
    let provider = Arc::new(FindProvider {
        matches: vec!["1.2.3.4.1".to_string(), "1.2.3.4.2".to_string()],
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .expect("bind");
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
    let responses = client
        .send_c_find(&CFindRequest::study_query(1))
        .await
        .expect("C-FIND");

    let matches: Vec<String> = responses
        .iter()
        .filter_map(|response| response.identifier.as_ref())
        .filter_map(|identifier| identifier.try_get_str(Tag::STUDY_INSTANCE_UID))
        .map(|uid| uid.trim().to_string())
        .collect();
    assert_eq!(
        matches,
        vec!["1.2.3.4.1".to_string(), "1.2.3.4.2".to_string()],
        "every match crosses the wire"
    );
    assert_eq!(responses.last().unwrap().status, 0x0000, "C-FIND completes");

    client.release().await;
    handle.await.unwrap().unwrap();
}

/// A C-GET SCP that hands back every instance it holds.
struct GetInstanceProvider {
    instances: Vec<DicomFile>,
}

#[async_trait]
impl ServiceProvider for GetInstanceProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![
            sop_class::STUDY_ROOT_QR_GET.to_string(),
            // the storage class the reverse C-STOREs travel on (§11.4)
            "1.2.840.10008.5.1.4.1.1.7".to_string(),
        ]
    }
    fn on_c_get<'a>(
        &'a self,
        _request: &'a CGetRequest,
    ) -> futures::stream::BoxStream<'a, radicom_network::Result<CGetEvent>> {
        let events: Vec<_> = self
            .instances
            .iter()
            .cloned()
            .map(|file| Ok(CGetEvent::instance(file)))
            .collect();
        Box::pin(futures::stream::iter(events))
    }
}

#[tokio::test]
async fn self_interop_c_get_receives_instances_over_tcp() {
    let provider = Arc::new(GetInstanceProvider {
        instances: vec![instance("1.2.3.4.5.1"), instance("1.2.3.4.5.2")],
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .expect("bind");
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let received: Arc<Mutex<Vec<DicomFile>>> = Arc::new(Mutex::new(Vec::new()));
    let mut client =
        DicomClient::new("SCU", "SCP", addr.to_string()).collect_reverse_stores(received.clone());
    let storage = vec!["1.2.840.10008.5.1.4.1.1.7".to_string()];
    let responses = client
        .send_c_get(&CGetRequest::study("1.2.3.4", 1), &storage)
        .await
        .expect("C-GET");

    assert_eq!(
        received.lock().unwrap().len(),
        2,
        "both instances arrived as reverse C-STOREs"
    );
    let terminal = responses.last().unwrap();
    assert_eq!(terminal.status, 0x0000, "C-GET completes");
    assert_eq!(terminal.summary.completed, 2);

    client.release().await;
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn self_interop_c_get_uses_registered_storage_classes() {
    // U1: declare the receivable storage classes once; `send_c_get_registered`
    // proposes them (with the SCP role) during the association negotiation.
    let provider = Arc::new(GetInstanceProvider {
        instances: vec![instance("1.2.3.4.6.1")],
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .expect("bind");
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let received: Arc<Mutex<Vec<DicomFile>>> = Arc::new(Mutex::new(Vec::new()));
    let mut client = DicomClient::new("SCU", "SCP", addr.to_string())
        .collect_reverse_stores(received.clone())
        .with_reverse_store_classes(vec!["1.2.840.10008.5.1.4.1.1.7".to_string()]);
    let responses = client
        .send_c_get_registered(&CGetRequest::study("1.2.3.4", 2))
        .await
        .expect("C-GET via registered classes");

    assert_eq!(received.lock().unwrap().len(), 1, "the instance arrived");
    assert_eq!(responses.last().unwrap().status, 0x0000, "C-GET completes");
    client.release().await;
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn c_get_registered_requires_declared_storage_classes() {
    let mut client = DicomClient::new("SCU", "SCP", "127.0.0.1:1").with_reverse_store_handler(
        Arc::new(|request: &CStoreRequest| Ok(CStoreResponse::success(request))),
    );
    match client
        .send_c_get_registered(&CGetRequest::study("1.2.3", 1))
        .await
    {
        Ok(_) => panic!("expected an error without declared classes"),
        Err(error) => assert!(error.to_string().contains("storage SOP classes"), "{error}"),
    }
}

/// A C-GET of a 4 MiB instance exercises the reverse-store receive spool end to
/// end: the instance is split over many PDVs, staged to a temp file on the
/// client, then decoded and captured intact (N4).
#[tokio::test]
async fn self_interop_c_get_large_instance_round_trips_through_the_spool() {
    let rows = 2048u16;
    let columns = 2048u16;
    let provider = Arc::new(GetInstanceProvider {
        instances: vec![large_instance("1.2.3.4.7.1", rows, columns)],
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .expect("bind");
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let received: Arc<Mutex<Vec<DicomFile>>> = Arc::new(Mutex::new(Vec::new()));
    let mut client =
        DicomClient::new("SCU", "SCP", addr.to_string()).collect_reverse_stores(received.clone());
    let storage = vec!["1.2.840.10008.5.1.4.1.1.7".to_string()];
    let responses = client
        .send_c_get(&CGetRequest::study("1.2.3.4", 1), &storage)
        .await
        .expect("C-GET large instance");

    assert_eq!(responses.last().unwrap().status, 0x0000, "C-GET completes");
    let frame_size = {
        let files = received.lock().unwrap();
        assert_eq!(files.len(), 1, "the large instance arrived once");
        let buffer = files[0]
            .dataset
            .get_element(Tag::PIXEL_DATA)
            .unwrap()
            .value()
            .as_buffer()
            .unwrap();
        assert!(
            !buffer.is_memory(),
            "reverse C-STORE keeps large values file-backed"
        );
        let view = radicom::pixel::PixelDataRef::from_dataset(&files[0].dataset).unwrap();
        view.get_frame(0).expect("one frame").size()
    };
    assert_eq!(
        frame_size,
        u64::from(rows) * u64::from(columns),
        "the whole 4 MiB frame survived the spooled round trip"
    );
    client.release().await;
    handle.await.unwrap().unwrap();
}

/// An SCP that performs C-MOVE by pushing every instance to a destination.
struct MoveToDestinationProvider {
    destination: std::net::SocketAddr,
    instances: Vec<DicomFile>,
}

#[async_trait]
impl ServiceProvider for MoveToDestinationProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![sop_class::STUDY_ROOT_QR_MOVE.to_string()]
    }
    fn on_c_move<'a>(
        &'a self,
        request: &'a CMoveRequest,
    ) -> futures::stream::BoxStream<'a, radicom_network::Result<CMoveResponse>> {
        let destination = self.destination;
        let instances = self.instances.clone();
        Box::pin(futures::stream::once(async move {
            let (summary, error) = radicom_network::cmove::store_to_destination(
                destination,
                "MOVE-SCP",
                "STORE-SCP",
                &instances,
                None,
                ClientOptions::default(),
            )
            .await?;
            let response = match error {
                None => CMoveResponse::completed(request, summary),
                Some(error) => {
                    let mut response = CMoveResponse::failure(
                        request,
                        radicom_network::dimse::status::PROCESSING_FAILURE,
                        error.to_string(),
                    );
                    response.summary = summary;
                    response
                }
            };
            Ok(response)
        }))
    }
}

#[tokio::test]
async fn self_interop_c_move_delivers_instances_to_a_destination_over_tcp() {
    // The destination is a real, separate C-STORE SCP on loopback.
    let destination_provider = Arc::new(CaptureProvider::new(vec![
        "1.2.840.10008.5.1.4.1.1.7".to_string()
    ]));
    let destination = DicomServer::bind(
        "127.0.0.1:0",
        destination_provider.clone(),
        ServerOptions {
            store_sink: StoreSink::Memory,
            ..Default::default()
        },
    )
    .await
    .expect("bind destination");
    let destination_addr = destination.local_addr();
    tokio::spawn(async move {
        let _ = destination.run().await;
    });

    let provider = Arc::new(MoveToDestinationProvider {
        destination: destination_addr,
        instances: vec![instance("1.2.3.4.5.1"), instance("1.2.3.4.5.2")],
    });
    let server = DicomServer::bind("127.0.0.1:0", provider, ServerOptions::default())
        .await
        .expect("bind");
    let addr = server.local_addr();
    let handle = tokio::spawn(async move { server.accept_one().await });

    let mut client = DicomClient::new("SCU", "SCP", addr.to_string());
    let responses = client
        .send_c_move(&CMoveRequest::study("1.2.3.4", "STORE-SCP", 1))
        .await
        .expect("C-MOVE");

    let terminal = responses.last().unwrap();
    assert_eq!(terminal.status, 0x0000, "C-MOVE completes");
    assert_eq!(terminal.summary.completed, 2, "both sub-operations counted");
    assert_eq!(terminal.remaining, 0);
    assert_eq!(
        destination_provider.files.lock().unwrap().len(),
        2,
        "the destination received every pushed instance"
    );

    client.release().await;
    handle.await.unwrap().unwrap();
}
