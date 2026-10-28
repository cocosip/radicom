//! External-peer interop (联调) test — opt-in via environment, hermetic otherwise.
//!
//! Normal `cargo test` skips it (no peer reachable in CI). To run against a real
//! DICOM peer (e.g. a fo-dicom C-STORE SCP):
//!
//! ```pwsh
//! $env:RADICOM_PEER   = "127.0.0.1:11112"   # required to run
//! $env:RADICOM_CALLING= "RADICOM"           # optional
//! $env:RADICOM_CALLED = "DCM4CHE"           # optional
//! $env:RADICOM_STORE_FILE = "tests-data\explicit-le\GH178.dcm"  # optional C-STORE
//! cargo test -p radicom-network --test interop -- --nocapture
//! ```

use radicom_network::association::sop_class;
use radicom_network::client::DicomClient;

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

#[tokio::test]
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
