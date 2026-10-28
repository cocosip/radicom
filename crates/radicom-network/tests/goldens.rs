//! PDU byte-stream golden baselines (docs/02-development-guide.md §5 §3/§4).
//!
//! The hex fixtures under `tests-data/goldens/` are produced by
//! `cargo xtask regen-goldens`. Whitespace is ignored, so the comparison pins the
//! exact encoded byte sequence regardless of line wrapping.

use std::fmt::Write as _;
use std::path::PathBuf;

use radicom_network::message::encode_command;
use radicom_network::pdu::encode;
use radicom_network::{Associate, Command, CommandField, PData, Pdu, Pdv, PresentationContext};

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
    encode(&Pdu::AssociateRq(associate))
}

fn fixture_pdata_cecho() -> Vec<u8> {
    let mut command = Command::request(CommandField::CEchoRq, 1);
    command.affected_sop_class_uid = Some("1.2.840.10008.1.1".to_string());
    let command_bytes = encode_command(&command.to_dataset()).expect("encode command");
    let pdv = Pdv { context_id: 1, is_command: true, is_last: true, data: command_bytes };
    encode(&Pdu::PData(PData { pdvs: vec![pdv] }))
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
