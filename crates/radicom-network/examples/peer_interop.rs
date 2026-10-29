//! External-peer interop smoke tool (联调).
//!
//! Runs against a real DICOM peer (e.g. a fo-dicom C-STORE SCP or a Q/R SCP):
//! an optional C-ECHO, plus optional C-STORE / C-FIND / C-GET / C-MOVE.
//!
//! Usage:
//!   cargo run -p radicom-network --example peer_interop -- \
//!     --peer 127.0.0.1:11112 --calling RADICOM --called DCM4CHE \
//!     [--no-echo] \
//!     [--store tests-data\explicit-le\GH487.dcm] \
//!     [--find STUDY --patient-name "Doe*"] \
//!     [--get STUDY --study-uid 1.2.3 --storage 1.2.840.10008.5.1.4.1.1.2,...] \
//!     [--move STUDY --study-uid 1.2.3 --dest ARCHIVE]
//!
//! Levels for --find/--get/--move: PATIENT | STUDY | SERIES | IMAGE. Filters are
//! optional. Each phase uses its own association, so a peer that rejects one
//! service does not poison the next. The process exits non-zero when a
//! *requested* operation fails.

use radicom::io::file::DicomFile;
use radicom::model::tag::Tag;
use radicom::model::vr::Vr;
use radicom_network::association::sop_class;
use radicom_network::cfind::{CFindRequest, QueryRetrieveLevel, QueryRoot};
use radicom_network::cget::CGetRequest;
use radicom_network::client::DicomClient;
use radicom_network::cmove::CMoveRequest;
use std::sync::{Arc, Mutex};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn client(calling: &str, called: &str, peer: &str) -> DicomClient {
    DicomClient::new(calling.to_string(), called.to_string(), peer.to_string())
}

/// The optional matching keys shared by FIND / GET / MOVE.
fn filter_pairs(args: &[String]) -> Vec<(u16, u16, Vr, String)> {
    let mut keys = Vec::new();
    if let Some(v) = arg(args, "--patient-name") {
        keys.push((0x0010, 0x0010, Vr::Pn, v));
    }
    if let Some(v) = arg(args, "--patient-id") {
        keys.push((0x0010, 0x0020, Vr::Lo, v));
    }
    if let Some(v) = arg(args, "--study-uid") {
        keys.push((0x0020, 0x000D, Vr::Ui, v));
    }
    if let Some(v) = arg(args, "--accession") {
        keys.push((0x0008, 0x0050, Vr::Sh, v));
    }
    keys
}

fn default_storage() -> Vec<String> {
    // CT / MR / SC cover most retrieve tests; override with --storage.
    vec![
        "1.2.840.10008.5.1.4.1.1.2".to_string(),
        "1.2.840.10008.5.1.4.1.1.4".to_string(),
        "1.2.840.10008.5.1.4.1.1.7".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::echo_is_fatal;

    #[test]
    fn a_refused_echo_only_fails_a_verification_only_run() {
        assert!(!echo_is_fatal(0x0000, false), "success never fails the run");
        assert!(echo_is_fatal(0x0122, false), "a lone refused echo is fatal");
        assert!(
            !echo_is_fatal(0x0122, true),
            "a real operation still gets its chance"
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let peer = arg(&args, "--peer").unwrap_or_else(|| "127.0.0.1:11112".into());
    let calling = arg(&args, "--calling").unwrap_or_else(|| "RADICOM".into());
    let called = arg(&args, "--called").unwrap_or_else(|| "ANY-SCP".into());
    println!("peer={peer} calling={calling} called={called}");

    let no_echo = args.iter().any(|a| a == "--no-echo");
    let store = arg(&args, "--store").or_else(|| std::env::var("RADICOM_STORE_FILE").ok());
    let find = arg(&args, "--find").or_else(|| std::env::var("RADICOM_FIND_LEVEL").ok());
    let get = arg(&args, "--get");
    let move_level = arg(&args, "--move");
    let has_real_op = store.is_some() || find.is_some() || get.is_some() || move_level.is_some();
    let mut failed = false;

    // 1) C-ECHO (Verification). Not every SCP supports it, so it is only fatal
    //    when it is the sole requested operation.
    if !no_echo {
        let mut echo_client = client(&calling, &called, &peer);
        match echo_client.send_c_echo(sop_class::VERIFICATION).await {
            Ok(echo) => {
                println!("C-ECHO status=0x{:04X} {}", echo.status, ok(echo.status));
                // verification answers 0x0122 on many PACSs; that is not a
                // reason to fail a run whose real work still succeeds (NET-25)
                failed |= echo_is_fatal(echo.status, has_real_op);
            }
            Err(e) if has_real_op => println!("C-ECHO unsupported by peer (ignored): {e}"),
            Err(e) => return Err(e.into()),
        }
        echo_client.release().await;
    }

    // 2) optional C-STORE (own association).
    if let Some(path) = store {
        let file = DicomFile::open(&path)?;
        let sop = file
            .dataset
            .try_get_str(Tag::SOP_CLASS_UID)
            .unwrap_or_default();
        println!("C-STORE {path} (SOP {sop})");
        let mut store_client = client(&calling, &called, &peer);
        let resp = store_client.send_c_store(&file).await?;
        println!("C-STORE status=0x{:04X} {}", resp.status, ok(resp.status));
        failed |= resp.status != 0;
        store_client.release().await;
    }

    // 3) optional C-FIND (own association).
    if let Some(level_text) = find {
        let level = QueryRetrieveLevel::parse(&level_text).ok_or("bad --find level")?;
        let mut request = CFindRequest::new(level, QueryRoot::Study, 1);
        for (group, element, vr, value) in filter_pairs(&args) {
            request = request.with_key(group, element, vr, value);
        }
        let mut find_client = client(&calling, &called, &peer);
        let responses = find_client.send_c_find(&request).await?;
        let matches: Vec<_> = responses.iter().filter(|r| r.is_pending()).collect();
        println!("C-FIND level={level_text} matches={}", matches.len());
        for response in matches {
            if let Some(id) = &response.identifier {
                let name = id.try_get_str(Tag::PATIENT_NAME).unwrap_or_default();
                let study = id.try_get_str(Tag::STUDY_INSTANCE_UID).unwrap_or_default();
                println!("  - PatientName={name:?} StudyInstanceUID={study:?}");
            }
        }
        let final_status = responses.last().map(|r| r.status).unwrap_or(0xFFFF);
        println!("C-FIND final status=0x{final_status:04X}");
        failed |= final_status != 0x0000;
        find_client.release().await;
    }

    // 4) optional C-GET (own association; instances arrive as reverse C-STORE).
    if let Some(level_text) = get {
        let level = QueryRetrieveLevel::parse(&level_text).ok_or("bad --get level")?;
        let mut request = CGetRequest::new(level, QueryRoot::Study, 1)?;
        for (group, element, vr, value) in filter_pairs(&args) {
            request = request.with_key(group, element, vr, value);
        }
        let storage: Vec<String> = arg(&args, "--storage")
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_else(default_storage);
        let received = Arc::new(Mutex::new(Vec::new()));
        let mut get_client = DicomClient::new(calling.clone(), called.clone(), peer.clone())
            .collect_reverse_stores(received.clone());
        let responses = get_client.send_c_get(&request, &storage).await?;
        let final_status = responses.last().map(|r| r.status).unwrap_or(0xFFFF);
        {
            let received = received.lock().unwrap();
            println!("C-GET level={level_text} received={}", received.len());
            for file in received.iter().take(10) {
                let sop = file
                    .dataset
                    .try_get_str(Tag::SOP_CLASS_UID)
                    .unwrap_or_default();
                let instance = file
                    .dataset
                    .try_get_str(Tag::SOP_INSTANCE_UID)
                    .unwrap_or_default();
                println!("  - SOP={sop} instance={instance}");
            }
        }
        if let Some(last) = responses.last() {
            println!(
                "C-GET sub-ops ok={} failed={} warning={}",
                last.summary.completed, last.summary.failed, last.summary.warning
            );
        }
        println!("C-GET final status=0x{final_status:04X}");
        failed |= final_status != 0x0000;
        get_client.release().await;
    }

    // 5) optional C-MOVE (own association; the SCP delivers to --dest).
    if let Some(level_text) = move_level {
        let dest = arg(&args, "--dest").ok_or("--move requires --dest AE")?;
        let level = QueryRetrieveLevel::parse(&level_text).ok_or("bad --move level")?;
        let mut request = CMoveRequest::new(level, QueryRoot::Study, &dest, 1)?;
        for (group, element, vr, value) in filter_pairs(&args) {
            request = request.with_key(group, element, vr, value);
        }
        let mut move_client = client(&calling, &called, &peer);
        let responses = move_client.send_c_move(&request).await?;
        let final_status = responses.last().map(|r| r.status).unwrap_or(0xFFFF);
        println!("C-MOVE level={level_text} dest={dest}");
        if let Some(last) = responses.last() {
            println!(
                "C-MOVE sub-ops ok={} failed={} warning={}",
                last.summary.completed, last.summary.failed, last.summary.warning
            );
        }
        println!("C-MOVE final status=0x{final_status:04X}");
        failed |= final_status != 0x0000;
        move_client.release().await;
    }

    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// True when a C-ECHO status must fail the process (NET-25).
///
/// A peer that refuses Verification is only fatal when Verification was the
/// whole point of the run.
fn echo_is_fatal(status: u16, has_real_op: bool) -> bool {
    status != 0 && !has_real_op
}

fn ok(status: u16) -> &'static str {
    if status == 0x0000 {
        "OK"
    } else {
        "FAIL"
    }
}
