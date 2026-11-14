//! Regression cases from DICOM PS3.7 2025d chapters 9/10 and PS3.4 C.4.

use radicom::{Dataset, Tag};
use radicom_network::cfind::CFindResponse;
use radicom_network::cget::CGetResponse;
use radicom_network::cmove::CMoveResponse;
use radicom_network::dimse::{status, Command, CommandField};
use radicom_network::nservice::{NServiceRequest, NServiceResponse};
use radicom_network::services::{CEchoResponse, CStoreResponse};

fn response(field: CommandField, code: u16) -> Command {
    let mut command = Command::request(field, 0);
    command.message_id_being_responded_to = Some(0);
    command.status = Some(code);
    command.no_dataset = true;
    command
}

#[test]
fn every_non_null_dataset_type_including_fo_dicom_0202_indicates_a_dataset() {
    for code in [0x0000u16, 0x0001, 0x0202, 0xFFFF] {
        let command = Command::request(CommandField::CStoreRq, 1);
        let mut dataset = command.to_dataset();
        dataset.insert_value(Tag::COMMAND_DATA_SET_TYPE, code);
        assert!(!Command::from_dataset(&dataset).unwrap().no_dataset);
    }
}

#[test]
fn zero_message_ids_are_valid_but_absent_ids_are_not() {
    let mut command = Command::request(CommandField::CEchoRq, 0);
    command.no_dataset = true;
    let mut dataset = command.to_dataset();
    assert_eq!(Command::from_dataset(&dataset).unwrap().message_id, 0);
    dataset.remove(&Tag::MESSAGE_ID);
    assert!(Command::from_dataset(&dataset).is_err());

    for field in [CommandField::CEchoRsp, CommandField::CCancelRq] {
        let mut dataset = response(field, status::SUCCESS).to_dataset();
        assert_eq!(
            Command::from_dataset(&dataset)
                .unwrap()
                .message_id_being_responded_to,
            Some(0)
        );
        dataset.remove(&Tag::MESSAGE_ID_BEING_RESPONDED_TO);
        assert!(Command::from_dataset(&dataset).is_err());
    }
}

#[test]
fn optional_response_uids_can_be_absent_with_zero_response_id() {
    assert!(CEchoResponse::from_command(&response(CommandField::CEchoRsp, 0)).is_ok());
    assert!(CStoreResponse::from_command(&response(CommandField::CStoreRsp, 0)).is_ok());
    assert!(CFindResponse::from_command(&response(CommandField::CFindRsp, 0), None).is_ok());
    assert!(CGetResponse::from_command(&response(CommandField::CGetRsp, 0)).is_ok());
    assert!(CMoveResponse::from_command(&response(CommandField::CMoveRsp, 0)).is_ok());
}

#[test]
fn absent_response_uids_stay_absent_when_reencoded() {
    let commands = [
        CEchoResponse::from_command(&response(CommandField::CEchoRsp, 0))
            .unwrap()
            .to_command(),
        CStoreResponse::from_command(&response(CommandField::CStoreRsp, 0))
            .unwrap()
            .to_command(),
        CFindResponse::from_command(&response(CommandField::CFindRsp, 0), None)
            .unwrap()
            .to_command(),
        CGetResponse::from_command(&response(CommandField::CGetRsp, 0))
            .unwrap()
            .to_command(),
        CMoveResponse::from_command(&response(CommandField::CMoveRsp, 0))
            .unwrap()
            .to_command(),
    ];
    for command in commands {
        assert_eq!(command.affected_sop_class_uid, None);
        assert_eq!(command.affected_sop_instance_uid, None);
    }
}

#[test]
fn retrieve_terminal_counters_are_optional_for_every_terminal_status() {
    for field in [CommandField::CGetRsp, CommandField::CMoveRsp] {
        for code in [status::SUCCESS, 0xFE00, 0xB000, 0xA701, 0xA801, 0xC000] {
            let mut command = response(field, code);
            command.message_id_being_responded_to = Some(1);
            command.affected_sop_class_uid = Some("1.2.3".into());
            if field == CommandField::CGetRsp {
                assert!(CGetResponse::from_command(&command).is_ok(), "{code:04X}");
            } else {
                assert!(CMoveResponse::from_command(&command).is_ok(), "{code:04X}");
            }
        }
    }
}

#[test]
fn pending_retrieve_requires_every_suboperation_counter() {
    for field in [CommandField::CGetRsp, CommandField::CMoveRsp] {
        let mut command = response(field, status::PENDING);
        command.remaining_suboperations = Some(1);
        command.completed_suboperations = Some(0);
        command.failed_suboperations = Some(0);
        command.warning_suboperations = Some(0);
        for missing in 0..4 {
            let mut incomplete = command.clone();
            match missing {
                0 => incomplete.remaining_suboperations = None,
                1 => incomplete.completed_suboperations = None,
                2 => incomplete.failed_suboperations = None,
                _ => incomplete.warning_suboperations = None,
            }
            let rejected = if field == CommandField::CGetRsp {
                CGetResponse::from_command(&incomplete).is_err()
            } else {
                CMoveResponse::from_command(&incomplete).is_err()
            };
            assert!(rejected);
        }
    }
}

#[test]
fn n_action_and_event_replies_echo_the_type_only_for_their_service() {
    for (field, expected_field) in [
        (CommandField::NActionRq, CommandField::NActionRsp),
        (CommandField::NEventReportRq, CommandField::NEventReportRsp),
    ] {
        let mut request = NServiceRequest::new(field, 0);
        request.action_type_id = Some(7);
        request.event_type_id = Some(3);
        let mut reply = Dataset::new();
        reply.insert_str(Tag::PATIENT_ID, radicom::Vr::Lo, "reply");
        let command = NServiceResponse::success(&request, Some(reply)).to_command();
        assert_eq!(command.field, expected_field);
        assert!(!command.no_dataset);
        if field == CommandField::NActionRq {
            assert_eq!(command.action_type_id, Some(7));
            assert_eq!(command.event_type_id, None);
        } else {
            assert_eq!(command.event_type_id, Some(3));
            assert_eq!(command.action_type_id, None);
        }
        let failure = NServiceResponse::failure(&request, status::PROCESSING_FAILURE).to_command();
        // PS3.7 / §11.3: a failure reply still carries the Type ID its own
        // service supplied, and never the other service's.
        if field == CommandField::NActionRq {
            assert_eq!(failure.action_type_id, Some(7));
            assert_eq!(failure.event_type_id, None);
        } else {
            assert_eq!(failure.event_type_id, Some(3));
            assert_eq!(failure.action_type_id, None);
        }
    }
}
