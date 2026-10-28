//! PDU byte-stream golden baselines (docs/02-development-guide.md §5 §3/§4).
//!
//! The hex fixtures under `tests-data/goldens/` are produced by
//! `cargo xtask regen-goldens`. Whitespace is ignored, so the comparison pins the
//! exact encoded byte sequence regardless of line wrapping.

use std::fmt::Write as _;
use std::path::PathBuf;

use radicom::model::dataset::Dataset;
use radicom_network::message::encode_command;
use radicom_network::pdu::encode;
use radicom_network::server::command_bytes;
use radicom_network::{
    Associate, CFindRequest, CFindResponse, Command, CommandField, PData, Pdu, Pdv, PresentationContext,
};
use radicom_network::nservice::NServiceRequest;

fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests-data/goldens")
}

fn hex_lines(bytes: &[u8]) -> String {
    let mut s = String::new();
    for chunk in bytes.chunks(16) {
        for (i, b) in chunk.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            let _ = write!(s, "{b:02X}");
        }
        s.push('\n');
    }
    s
}

fn normalized(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_uppercase()
}

// Fixtures — keep in sync with `regen_goldens` in xtask/src/main.rs.
fn fixture_associate_rq() -> Vec<u8> {
    let mut associate = Associate::new("RADICOM_SCP", "RADICOM_SCU");
    associate.presentation_contexts.push(PresentationContext::new(
        1,
        "1.2.840.10008.1.1",
        vec!["1.2.840.10008.1.2.1".to_string()],
    ));
    encode(&Pdu::AssociateRq(associate)).expect("encode")
}

fn fixture_pdata_cecho() -> Vec<u8> {
    let mut command = Command::request(CommandField::CEchoRq, 1);
    command.affected_sop_class_uid = Some("1.2.840.10008.1.1".to_string());
    let command_bytes = encode_command(&command.to_dataset()).expect("encode command");
    let pdv = Pdv { context_id: 1, is_command: true, is_last: true, data: command_bytes.into() };
    encode(&Pdu::PData(PData { pdvs: vec![pdv] })).expect("encode")
}

/// The association request as `Association::open` builds it (user information
/// included): pins that a real RQ carries Implementation Class UID (0x52) and
/// Version Name (0x55), not an empty user item (§2.19, §4.10).
fn fixture_associate_rq_user_info() -> Vec<u8> {
    let mut associate = Associate::new("RADICOM_SCP", "RADICOM_SCU");
    associate.presentation_contexts.push(PresentationContext::new(
        1,
        "1.2.840.10008.1.1",
        vec!["1.2.840.10008.1.2.1".to_string()],
    ));
    associate.user_information = radicom_network::association::build_user_information(
        16384,
        "1.2.826.0.1.3680043.10.1330.2",
        "RADICOM_0_1",
        Vec::new(),
    );
    encode(&Pdu::AssociateRq(associate)).expect("encode")
}

/// A command set for an N-service request (N-GET uses Requested UIDs).
fn fixture_command_n_get_rq() -> Vec<u8> {
    let mut request = NServiceRequest::new(CommandField::NGetRq, 7);
    request.sop_class_uid = Some("1.2.840.10008.1.20.1".to_string());
    request.sop_instance_uid = Some("1.2.3.4.5".to_string());
    command_bytes(&request.to_command()).expect("encode command")
}

/// The command set the SCP sends as a reverse C-STORE sub-operation during a
/// C-GET (§7.4): a C-STORE-RQ naming the instance as Affected.
fn fixture_command_cget_reverse_store_rq() -> Vec<u8> {
    let mut command = Command::request(CommandField::CStoreRq, 1);
    command.affected_sop_class_uid = Some("1.2.840.10008.1.2.4.50".to_string());
    command.affected_sop_instance_uid = Some("1.2.3.4.5.6".to_string());
    command.no_dataset = false;
    command_bytes(&command).expect("encode command")
}

/// A Pending C-FIND response command set (status 0xFF00) carrying no dataset
/// in the command itself; the match Identifier follows in the data PDV.
fn fixture_command_cfind_pending_rsp() -> Vec<u8> {
    let request = CFindRequest::study_query(1);
    let response = CFindResponse::pending(&request, Dataset::new());
    command_bytes(&response.to_command()).expect("encode command")
}

fn check(golden: &str, bytes: Vec<u8>) {
    let produced = hex_lines(&bytes);
    let expected = std::fs::read_to_string(goldens_dir().join(golden))
        .unwrap_or_else(|e| panic!("read golden {golden}: {e}"));
    assert_eq!(
        normalized(&produced),
        normalized(&expected),
        "PDU golden {golden} is stale; run `cargo xtask regen-goldens`"
    );
}

#[test]
fn pdu_goldens_are_current() {
    check("associate_rq.hex", fixture_associate_rq());
    check("pdata_cecho.hex", fixture_pdata_cecho());
}

#[test]
fn service_command_goldens_are_current() {
    check("cmd_n_get_rq.hex", fixture_command_n_get_rq());
    check("cmd_cget_reverse_store_rq.hex", fixture_command_cget_reverse_store_rq());
    check("cmd_cfind_pending_rsp.hex", fixture_command_cfind_pending_rsp());
}

#[test]
fn real_association_request_carries_implementation_identity() {
    // §2.19/§4.10: the user information of a real RQ must carry Implementation
    // Class UID (0x52) and Version Name (0x55), not just Max PDU Length.
    let bytes = fixture_associate_rq_user_info();
    let Pdu::AssociateRq(request) = radicom_network::pdu::decode(&bytes).expect("decode RQ") else {
        panic!("expected an A-ASSOCIATE-RQ");
    };
    assert_eq!(
        request.user_information.implementation_class_uid.as_deref(),
        Some("1.2.826.0.1.3680043.10.1330.2")
    );
    assert_eq!(request.user_information.implementation_version_name.as_deref(), Some("RADICOM_0_1"));
    check("associate_rq_user_info.hex", bytes);
}
