//! DIMSE command model and status codes (DIMSE-01; docs/07-network.md §6).
//!
//! Commands always travel as Implicit VR Little Endian datasets. This module
//! builds and parses the command set; the dataset that follows a command is
//! handled by the protocol engine.

use crate::error::{NetworkError, Result};
use radicom::model::dataset::Dataset;
use radicom::model::vr::Vr;

/// Command set tags (§6.1).
pub mod tag {
    use radicom::model::tag::Tag;

    pub const COMMAND_GROUP_LENGTH: Tag = Tag::new(0x0000, 0x0000);
    pub const AFFECTED_SOP_CLASS_UID: Tag = Tag::new(0x0000, 0x0002);
    pub const REQUESTED_SOP_CLASS_UID: Tag = Tag::new(0x0000, 0x0003);
    pub const COMMAND_FIELD: Tag = Tag::new(0x0000, 0x0100);
    pub const MESSAGE_ID: Tag = Tag::new(0x0000, 0x0110);
    pub const MESSAGE_ID_BEING_RESPONDED_TO: Tag = Tag::new(0x0000, 0x0120);
    pub const MOVE_DESTINATION: Tag = Tag::new(0x0000, 0x0600);
    pub const PRIORITY: Tag = Tag::new(0x0000, 0x0700);
    pub const COMMAND_DATA_SET_TYPE: Tag = Tag::new(0x0000, 0x0800);
    pub const STATUS: Tag = Tag::new(0x0000, 0x0900);
    pub const ERROR_COMMENT: Tag = Tag::new(0x0000, 0x0902);
    pub const AFFECTED_SOP_INSTANCE_UID: Tag = Tag::new(0x0000, 0x1000);
    pub const REQUESTED_SOP_INSTANCE_UID: Tag = Tag::new(0x0000, 0x1001);
    pub const EVENT_TYPE_ID: Tag = Tag::new(0x0000, 0x1002);
    pub const ATTRIBUTE_IDENTIFIER_LIST: Tag = Tag::new(0x0000, 0x1005);
    pub const ACTION_TYPE_ID: Tag = Tag::new(0x0000, 0x1008);
    pub const NUMBER_OF_REMAINING_SUBOPERATIONS: Tag = Tag::new(0x0000, 0x1020);
    pub const NUMBER_OF_COMPLETED_SUBOPERATIONS: Tag = Tag::new(0x0000, 0x1021);
    pub const NUMBER_OF_FAILED_SUBOPERATIONS: Tag = Tag::new(0x0000, 0x1022);
    pub const NUMBER_OF_WARNING_SUBOPERATIONS: Tag = Tag::new(0x0000, 0x1023);
    pub const MOVE_ORIGINATOR_APPLICATION_ENTITY_TITLE: Tag = Tag::new(0x0000, 0x1030);
    pub const MOVE_ORIGINATOR_MESSAGE_ID: Tag = Tag::new(0x0000, 0x1031);
}

/// `(0000,0800)` value meaning "no dataset follows" (§5.2).
pub const NO_DATA_SET: u16 = 0x0101;

/// Priority element values; the numeric order is low(0) < high(1) < medium(2)
/// per the standard, which is not the intuitive order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Priority {
    Low,
    #[default]
    High,
    Medium,
}

impl Priority {
    pub fn code(self) -> u16 {
        match self {
            Priority::Low => 0,
            Priority::High => 1,
            Priority::Medium => 2,
        }
    }

    pub fn from_code(code: u16) -> Self {
        match code {
            0 => Priority::Low,
            2 => Priority::Medium,
            _ => Priority::High,
        }
    }
}

/// The 22 command fields (§6.1), requests `bit15 = 0`, responses `bit15 = 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandField {
    CStoreRq,
    CStoreRsp,
    CGetRq,
    CGetRsp,
    CFindRq,
    CFindRsp,
    CMoveRq,
    CMoveRsp,
    CEchoRq,
    CEchoRsp,
    CCancelRq,
    NEventReportRq,
    NEventReportRsp,
    NGetRq,
    NGetRsp,
    NSetRq,
    NSetRsp,
    NActionRq,
    NActionRsp,
    NCreateRq,
    NCreateRsp,
    NDeleteRq,
    NDeleteRsp,
}

impl CommandField {
    pub fn code(self) -> u16 {
        match self {
            CommandField::CStoreRq => 0x0001,
            CommandField::CStoreRsp => 0x8001,
            CommandField::CGetRq => 0x0010,
            CommandField::CGetRsp => 0x8010,
            CommandField::CFindRq => 0x0020,
            CommandField::CFindRsp => 0x8020,
            CommandField::CMoveRq => 0x0021,
            CommandField::CMoveRsp => 0x8021,
            CommandField::CEchoRq => 0x0030,
            CommandField::CEchoRsp => 0x8030,
            CommandField::CCancelRq => 0x0FFF,
            CommandField::NEventReportRq => 0x0100,
            CommandField::NEventReportRsp => 0x8100,
            CommandField::NGetRq => 0x0110,
            CommandField::NGetRsp => 0x8110,
            CommandField::NSetRq => 0x0120,
            CommandField::NSetRsp => 0x8120,
            CommandField::NActionRq => 0x0130,
            CommandField::NActionRsp => 0x8130,
            CommandField::NCreateRq => 0x0140,
            CommandField::NCreateRsp => 0x8140,
            CommandField::NDeleteRq => 0x0150,
            CommandField::NDeleteRsp => 0x8150,
        }
    }

    pub fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            0x0001 => CommandField::CStoreRq,
            0x8001 => CommandField::CStoreRsp,
            0x0010 => CommandField::CGetRq,
            0x8010 => CommandField::CGetRsp,
            0x0020 => CommandField::CFindRq,
            0x8020 => CommandField::CFindRsp,
            0x0021 => CommandField::CMoveRq,
            0x8021 => CommandField::CMoveRsp,
            0x0030 => CommandField::CEchoRq,
            0x8030 => CommandField::CEchoRsp,
            0x0FFF => CommandField::CCancelRq,
            0x0100 => CommandField::NEventReportRq,
            0x8100 => CommandField::NEventReportRsp,
            0x0110 => CommandField::NGetRq,
            0x8110 => CommandField::NGetRsp,
            0x0120 => CommandField::NSetRq,
            0x8120 => CommandField::NSetRsp,
            0x0130 => CommandField::NActionRq,
            0x8130 => CommandField::NActionRsp,
            0x0140 => CommandField::NCreateRq,
            0x8140 => CommandField::NCreateRsp,
            0x0150 => CommandField::NDeleteRq,
            0x8150 => CommandField::NDeleteRsp,
            _ => return None,
        })
    }

    pub fn is_request(self) -> bool {
        self.code() & 0x8000 == 0
    }
}

/// A DIMSE command message: the typed view of a command dataset (§6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub field: CommandField,
    /// `(0000,0002)` for requests, echoed back on responses.
    pub affected_sop_class_uid: Option<String>,
    /// `(0000,0003)`, N-services only.
    pub requested_sop_class_uid: Option<String>,
    pub message_id: u16,
    /// `(0000,0120)`, responses only.
    pub message_id_being_responded_to: Option<u16>,
    /// `(0000,0900)`, responses only.
    pub status: Option<u16>,
    pub error_comment: Option<String>,
    pub priority: Priority,
    /// True when `(0000,0800)` says no dataset follows.
    pub no_dataset: bool,
    pub move_destination: Option<String>,
    pub affected_sop_instance_uid: Option<String>,
    pub requested_sop_instance_uid: Option<String>,
    pub event_type_id: Option<u16>,
    pub action_type_id: Option<u16>,
    pub move_originator_ae_title: Option<String>,
    pub move_originator_message_id: Option<u16>,
    pub remaining_suboperations: Option<u16>,
    pub completed_suboperations: Option<u16>,
    pub failed_suboperations: Option<u16>,
    pub warning_suboperations: Option<u16>,
}

impl Command {
    /// A minimal request of the given field.
    pub fn request(field: CommandField, message_id: u16) -> Self {
        Command {
            field,
            affected_sop_class_uid: None,
            requested_sop_class_uid: None,
            message_id,
            message_id_being_responded_to: None,
            status: None,
            error_comment: None,
            priority: Priority::default(),
            no_dataset: false,
            move_destination: None,
            affected_sop_instance_uid: None,
            requested_sop_instance_uid: None,
            event_type_id: None,
            action_type_id: None,
            move_originator_ae_title: None,
            move_originator_message_id: None,
            remaining_suboperations: None,
            completed_suboperations: None,
            failed_suboperations: None,
            warning_suboperations: None,
        }
    }

    /// A minimal response echoing `request`.
    pub fn response(request: &Command, status: u16) -> Self {
        let field = response_field(request.field).expect("request field has a response counterpart");
        let mut out = Command::request(field, request.message_id);
        out.message_id_being_responded_to = Some(request.message_id);
        out.status = Some(status);
        out.affected_sop_class_uid = request.affected_sop_class_uid.clone();
        out.no_dataset = true;
        out
    }

    /// Builds the command dataset, recomputing `(0000,0000)` (§5.3).
    pub fn to_dataset(&self) -> Dataset {
        let mut ds = Dataset::new();
        // required ordering: group length first, then ascending tags
        ds.insert_value(tag::COMMAND_FIELD, self.field.code());
        if let Some(uid) = &self.affected_sop_class_uid {
            ds.insert_str(tag::AFFECTED_SOP_CLASS_UID, Vr::Ui, uid);
        }
        if let Some(uid) = &self.requested_sop_class_uid {
            ds.insert_str(tag::REQUESTED_SOP_CLASS_UID, Vr::Ui, uid);
        }
        ds.insert_value(tag::MESSAGE_ID, self.message_id);
        if let Some(id) = self.message_id_being_responded_to {
            ds.insert_value(tag::MESSAGE_ID_BEING_RESPONDED_TO, id);
        }
        if let Some(status) = self.status {
            ds.insert_value(tag::STATUS, status);
        }
        if let Some(comment) = &self.error_comment {
            // LO is limited to 64 bytes
            ds.insert_str(tag::ERROR_COMMENT, Vr::Lo, truncate_lo(comment));
        }
        ds.insert_value(tag::PRIORITY, self.priority.code());
        if self.no_dataset {
            ds.insert_value(tag::COMMAND_DATA_SET_TYPE, NO_DATA_SET);
        } else {
            // 0x0001 = "dataset follows, not UID-addressed"
            ds.insert_value(tag::COMMAND_DATA_SET_TYPE, 0x0001u16);
        }
        if let Some(dest) = &self.move_destination {
            ds.insert_str(tag::MOVE_DESTINATION, Vr::Ae, dest);
        }
        if let Some(uid) = &self.affected_sop_instance_uid {
            ds.insert_str(tag::AFFECTED_SOP_INSTANCE_UID, Vr::Ui, uid);
        }
        if let Some(uid) = &self.requested_sop_instance_uid {
            ds.insert_str(tag::REQUESTED_SOP_INSTANCE_UID, Vr::Ui, uid);
        }
        if let Some(id) = self.event_type_id {
            ds.insert_value(tag::EVENT_TYPE_ID, id);
        }
        if let Some(id) = self.action_type_id {
            ds.insert_value(tag::ACTION_TYPE_ID, id);
        }
        if let Some(ae) = &self.move_originator_ae_title {
            ds.insert_str(tag::MOVE_ORIGINATOR_APPLICATION_ENTITY_TITLE, Vr::Ae, ae);
        }
        if let Some(id) = self.move_originator_message_id {
            ds.insert_value(tag::MOVE_ORIGINATOR_MESSAGE_ID, id);
        }
        if let Some(n) = self.remaining_suboperations {
            ds.insert_value(tag::NUMBER_OF_REMAINING_SUBOPERATIONS, n);
        }
        if let Some(n) = self.completed_suboperations {
            ds.insert_value(tag::NUMBER_OF_COMPLETED_SUBOPERATIONS, n);
        }
        if let Some(n) = self.failed_suboperations {
            ds.insert_value(tag::NUMBER_OF_FAILED_SUBOPERATIONS, n);
        }
        if let Some(n) = self.warning_suboperations {
            ds.insert_value(tag::NUMBER_OF_WARNING_SUBOPERATIONS, n);
        }

        // (0000,0000) is the byte length of the rest of the command group
        let mut with_len = ds.clone();
        with_len.insert_value(tag::COMMAND_GROUP_LENGTH, 0u32);
        let total = radicom::io::writer::DicomWriter::default()
            .dataset_length(&with_len, &radicom::model::transfer_syntax::syntaxes::IMPLICIT_VR_LITTLE_ENDIAN)
            .unwrap_or(0);
        // the group length element itself is 4 tag + 4 length + 4 value = 12
        ds.insert_value(tag::COMMAND_GROUP_LENGTH, (total.saturating_sub(12)) as u32);
        ds
    }

    /// Parses a command dataset (§5.2: always Implicit VR Little Endian).
    pub fn from_dataset(ds: &Dataset) -> Result<Self> {
        let code: u16 = ds
            .get(tag::COMMAND_FIELD)
            .map_err(|_| NetworkError::protocol("command set has no CommandField"))?;
        let field = CommandField::from_code(code)
            .ok_or(NetworkError::protocol("unknown command field"))?;

        let data_set_type: u16 = ds.get_or(tag::COMMAND_DATA_SET_TYPE, NO_DATA_SET);
        Ok(Command {
            field,
            affected_sop_class_uid: ds.try_get_str(tag::AFFECTED_SOP_CLASS_UID).map(|s| s.trim().to_string()),
            requested_sop_class_uid: ds.try_get_str(tag::REQUESTED_SOP_CLASS_UID).map(|s| s.trim().to_string()),
            message_id: ds.get_or(tag::MESSAGE_ID, 0u16),
            message_id_being_responded_to: ds.get(tag::MESSAGE_ID_BEING_RESPONDED_TO).ok(),
            status: ds.get(tag::STATUS).ok(),
            error_comment: ds.try_get_str(tag::ERROR_COMMENT).map(|s| s.trim_end().to_string()),
            priority: Priority::from_code(ds.get_or(tag::PRIORITY, 1u16)),
            no_dataset: data_set_type == NO_DATA_SET,
            move_destination: ds.try_get_str(tag::MOVE_DESTINATION).map(|s| s.trim().to_string()),
            affected_sop_instance_uid: ds.try_get_str(tag::AFFECTED_SOP_INSTANCE_UID).map(|s| s.trim().to_string()),
            requested_sop_instance_uid: ds.try_get_str(tag::REQUESTED_SOP_INSTANCE_UID).map(|s| s.trim().to_string()),
            event_type_id: ds.get(tag::EVENT_TYPE_ID).ok(),
            action_type_id: ds.get(tag::ACTION_TYPE_ID).ok(),
            move_originator_ae_title: ds
                .try_get_str(tag::MOVE_ORIGINATOR_APPLICATION_ENTITY_TITLE)
                .map(|s| s.trim().to_string()),
            move_originator_message_id: ds.get(tag::MOVE_ORIGINATOR_MESSAGE_ID).ok(),
            remaining_suboperations: ds.get(tag::NUMBER_OF_REMAINING_SUBOPERATIONS).ok(),
            completed_suboperations: ds.get(tag::NUMBER_OF_COMPLETED_SUBOPERATIONS).ok(),
            failed_suboperations: ds.get(tag::NUMBER_OF_FAILED_SUBOPERATIONS).ok(),
            warning_suboperations: ds.get(tag::NUMBER_OF_WARNING_SUBOPERATIONS).ok(),
        })
    }
}

/// The response counterpart of a request field.
pub fn response_field(request: CommandField) -> Option<CommandField> {
    Some(match request {
        CommandField::CStoreRq => CommandField::CStoreRsp,
        CommandField::CGetRq => CommandField::CGetRsp,
        CommandField::CFindRq => CommandField::CFindRsp,
        CommandField::CMoveRq => CommandField::CMoveRsp,
        CommandField::CEchoRq => CommandField::CEchoRsp,
        CommandField::NEventReportRq => CommandField::NEventReportRsp,
        CommandField::NGetRq => CommandField::NGetRsp,
        CommandField::NSetRq => CommandField::NSetRsp,
        CommandField::NActionRq => CommandField::NActionRsp,
        CommandField::NCreateRq => CommandField::NCreateRsp,
        CommandField::NDeleteRq => CommandField::NDeleteRsp,
        // responses and C-CANCEL have no counterpart
        _ => return None,
    })
}

/// Truncates an error comment to the LO length limit of 64 bytes (§7.2).
pub fn truncate_lo(comment: &str) -> String {
    const LIMIT: usize = 64;
    if comment.len() <= LIMIT {
        return comment.to_string();
    }
    // cut on a char boundary so multi-byte text stays valid
    let mut end = LIMIT;
    while end > 0 && !comment.is_char_boundary(end) {
        end -= 1;
    }
    comment[..end].to_string()
}

/// Classification of a status code (§6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusState {
    Success,
    Pending,
    Cancel,
    Warning,
    Failure,
}

impl StatusState {
    /// True when the peer will send more responses for this message.
    pub fn is_pending(self) -> bool {
        self == StatusState::Pending
    }
}

/// A status code with its classification and optional comment (§6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub code: u16,
    pub state: StatusState,
    pub comment: Option<String>,
}

impl Status {
    pub fn new(code: u16) -> Self {
        Status { code, state: classify(code), comment: None }
    }

    pub fn with_comment(code: u16, comment: impl Into<String>) -> Self {
        Status { code, state: classify(code), comment: Some(comment.into()) }
    }

    pub fn is_success(&self) -> bool {
        self.state == StatusState::Success
    }

    pub fn is_pending(&self) -> bool {
        self.state == StatusState::Pending
    }
}

/// Classifies a status code, resolving mask classes best-match (§6.2).
pub fn classify(code: u16) -> StatusState {
    // exact matches first
    if let Some(state) = exact_state(code) {
        return state;
    }
    // then the mask classes, most mask bits first (Bxxx warnings, Cxxx, etc.)
    if code & 0xF000 == 0xB000 {
        return StatusState::Warning;
    }
    if code & 0xF000 == 0xC000 {
        return StatusState::Failure;
    }
    if code & 0xFF00 == 0x0100 {
        // 0x0100..0x010F are the pending progress family; 0x0110 and above
        // in the same 0x01xx byte are failures (ProcessingFailure is 0x0110)
        if code & 0x00F0 == 0x0000 {
            return StatusState::Pending;
        }
    }
    if code & 0xFF00 == 0x0000 {
        return StatusState::Success;
    }
    // unknown codes fall back to ProcessingFailure classification
    StatusState::Failure
}

fn exact_state(code: u16) -> Option<StatusState> {
    Some(match code {
        0x0000 => StatusState::Success,
        0x0001 | 0x0002 | 0x0107 | 0x0108 => StatusState::Warning,
        // 0xFF00/0xFF01 are the "match found / no match" pending statuses;
        // 0x0100..0x0106 are the operation-progress pending family
        0x0100 | 0x0101 | 0x0102 | 0x0106 | 0xFF00 | 0xFF01 => StatusState::Pending,
        0xFE00 => StatusState::Cancel,
        _ => return None,
    })
}

/// The generic status codes (§6.2), usable as constants.
pub mod status {
    pub const SUCCESS: u16 = 0x0000;
    pub const CANCEL: u16 = 0xFE00;
    pub const PENDING: u16 = 0xFF00;
    pub const PENDING_WARNING: u16 = 0xFF01;
    pub const PROCESSING_FAILURE: u16 = 0x0110;
    pub const SOP_CLASS_NOT_SUPPORTED: u16 = 0x0122;
    pub const OUT_OF_RESOURCES: u16 = 0xA700;
    pub const OUT_OF_RESOURCES_2: u16 = 0xA701;
    pub const UNABLE_TO_PROCESS: u16 = 0xC000;
    pub const IDENTIFIER_DOES_NOT_MATCH_SOP_CLASS: u16 = 0xA900;
    pub const INVALID_ARGUMENT_VALUE: u16 = 0xA900;
    // storage
    pub const STORAGE_FAILURE: u16 = 0xA700;
    pub const STORAGE_OUT_OF_RESOURCES: u16 = 0xA700;
    pub const STORAGE_DATA_SET_DOES_NOT_MATCH_SOP_CLASS: u16 = 0xA900;
    // query/retrieve
    pub const REFUSED_OUT_OF_RESOURCES: u16 = 0xA700;
    pub const REFUSED_SOP_CLASS_NOT_SUPPORTED: u16 = 0x0122;
    pub const CANCELLED: u16 = 0xFE00;
    pub const NO_SUCH_SOP_INSTANCE: u16 = 0xA701;
    pub const MOVE_DESTINATION_UNKNOWN: u16 = 0xA801;
    pub const SUBOPERATIONS_COMPLETED_WITH_WARNINGS: u16 = 0xB000;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_fields_round_trip() {
        for field in [
            CommandField::CStoreRq,
            CommandField::CStoreRsp,
            CommandField::CGetRq,
            CommandField::CGetRsp,
            CommandField::CFindRq,
            CommandField::CFindRsp,
            CommandField::CMoveRq,
            CommandField::CMoveRsp,
            CommandField::CEchoRq,
            CommandField::CEchoRsp,
            CommandField::CCancelRq,
            CommandField::NEventReportRq,
            CommandField::NEventReportRsp,
            CommandField::NGetRq,
            CommandField::NGetRsp,
            CommandField::NSetRq,
            CommandField::NSetRsp,
            CommandField::NActionRq,
            CommandField::NActionRsp,
            CommandField::NCreateRq,
            CommandField::NCreateRsp,
            CommandField::NDeleteRq,
            CommandField::NDeleteRsp,
        ] {
            assert_eq!(CommandField::from_code(field.code()), Some(field), "{field:?}");
        }
        assert_eq!(CommandField::from_code(0x0099), None);
    }

    #[test]
    fn request_and_response_flags_follow_bit_15() {
        assert!(CommandField::CEchoRq.is_request());
        assert!(!CommandField::CEchoRsp.is_request());
        assert!(CommandField::CStoreRq.is_request());
        assert!(!CommandField::CStoreRsp.is_request());
    }

    #[test]
    fn priority_uses_the_standard_numeric_order() {
        // the standard order is low(0) < high(1) < medium(2), which is not
        // the intuitive ordering
        assert_eq!(Priority::Low.code(), 0);
        assert_eq!(Priority::High.code(), 1);
        assert_eq!(Priority::Medium.code(), 2);
        assert_eq!(Priority::from_code(0), Priority::Low);
        assert_eq!(Priority::from_code(1), Priority::High);
        assert_eq!(Priority::from_code(2), Priority::Medium);
        assert_eq!(Priority::default(), Priority::High);
    }

    #[test]
    fn echo_request_round_trips_through_a_dataset() {
        let mut request = Command::request(CommandField::CEchoRq, 42);
        request.affected_sop_class_uid = Some("1.2.840.10008.1.1".into());
        let ds = request.to_dataset();
        let back = Command::from_dataset(&ds).unwrap();
        assert_eq!(back.field, CommandField::CEchoRq);
        assert_eq!(back.message_id, 42);
        assert_eq!(back.affected_sop_class_uid.as_deref(), Some("1.2.840.10008.1.1"));
        assert!(!back.no_dataset, "C-ECHO request has no dataset, but the flag is set by the sender");
    }

    #[test]
    fn group_length_counts_the_rest_of_the_group() {
        let mut request = Command::request(CommandField::CStoreRq, 7);
        request.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.7".into());
        let ds = request.to_dataset();
        let declared: u32 = ds.get(tag::COMMAND_GROUP_LENGTH).unwrap();
        let total = radicom::io::writer::DicomWriter::default()
            .dataset_length(&ds, &radicom::model::transfer_syntax::syntaxes::IMPLICIT_VR_LITTLE_ENDIAN)
            .unwrap();
        assert_eq!(declared as u64, total - 12, "(0000,0000) covers everything after itself");
    }

    #[test]
    fn response_echoes_the_request() {
        let mut request = Command::request(CommandField::CFindRq, 11);
        request.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.2.1.1".into());
        let response = Command::response(&request, status::PENDING);
        assert_eq!(response.field, CommandField::CFindRsp);
        assert_eq!(response.message_id_being_responded_to, Some(11));
        assert_eq!(response.status, Some(status::PENDING));
        assert_eq!(
            response.affected_sop_class_uid.as_deref(),
            Some("1.2.840.10008.5.1.4.1.2.1.1"),
            "affected SOP class is echoed on responses"
        );

        let back = Command::from_dataset(&response.to_dataset()).unwrap();
        assert_eq!(back.field, CommandField::CFindRsp);
        assert_eq!(back.message_id_being_responded_to, Some(11));
        assert_eq!(back.status, Some(status::PENDING));
    }

    #[test]
    fn response_field_covers_every_request() {
        for field in [
            CommandField::CStoreRq,
            CommandField::CGetRq,
            CommandField::CFindRq,
            CommandField::CMoveRq,
            CommandField::CEchoRq,
            CommandField::NEventReportRq,
            CommandField::NGetRq,
            CommandField::NSetRq,
            CommandField::NActionRq,
            CommandField::NCreateRq,
            CommandField::NDeleteRq,
        ] {
            let response = response_field(field).unwrap_or_else(|| panic!("{field:?} has a response"));
            assert!(!response.is_request(), "{response:?} must be a response");
            assert_eq!(response.code() & 0x7FFF, field.code(), "{field:?} pairs with {response:?}");
        }
        assert_eq!(response_field(CommandField::CEchoRsp), None);
        assert_eq!(response_field(CommandField::CCancelRq), None);
    }

    #[test]
    fn dataset_type_flag_distinguishes_none_from_present() {
        let mut request = Command::request(CommandField::CStoreRq, 1);
        request.no_dataset = true;
        let ds = request.to_dataset();
        assert_eq!(ds.get::<u16>(tag::COMMAND_DATA_SET_TYPE).unwrap(), NO_DATA_SET);
        assert!(Command::from_dataset(&ds).unwrap().no_dataset);

        let mut request = Command::request(CommandField::CStoreRq, 1);
        request.no_dataset = false;
        let ds = request.to_dataset();
        assert_ne!(ds.get::<u16>(tag::COMMAND_DATA_SET_TYPE).unwrap(), NO_DATA_SET);
        assert!(!Command::from_dataset(&ds).unwrap().no_dataset);
    }

    #[test]
    fn error_comment_is_truncated_to_lo_length() {
        let long = "x".repeat(200);
        let truncated = truncate_lo(&long);
        assert_eq!(truncated.len(), 64);

        // multi-byte text is cut on a char boundary
        let wide = "é".repeat(100);
        let truncated = truncate_lo(&wide);
        assert!(truncated.len() <= 64);
        assert!(truncated.chars().all(|c| c == 'é'), "no broken characters");
        assert_eq!(truncate_lo("short"), "short");
    }

    #[test]
    fn status_classification_covers_the_families() {
        assert_eq!(classify(status::SUCCESS), StatusState::Success);
        assert_eq!(classify(status::PENDING), StatusState::Pending);
        assert_eq!(classify(status::CANCEL), StatusState::Cancel);
        assert_eq!(classify(status::SUBOPERATIONS_COMPLETED_WITH_WARNINGS), StatusState::Warning);
        assert_eq!(classify(status::PROCESSING_FAILURE), StatusState::Failure);

        // pending family
        assert_eq!(classify(0x0100), StatusState::Pending);
        assert_eq!(classify(0x0101), StatusState::Pending);
        assert_eq!(classify(0x0102), StatusState::Pending);
        assert_eq!(classify(0x0106), StatusState::Pending);
    }

    #[test]
    fn mask_classes_resolve_by_family() {
        // Bxxx is the warning mask class
        assert_eq!(classify(0xB000), StatusState::Warning);
        assert_eq!(classify(0xB001), StatusState::Warning);
        assert_eq!(classify(0xB123), StatusState::Warning);
        // Cxxx is the failure mask class
        assert_eq!(classify(0xC000), StatusState::Failure);
        assert_eq!(classify(0xC999), StatusState::Failure);
        // unknown codes fall back to failure
        assert_eq!(classify(0xE123), StatusState::Failure);
    }

    #[test]
    fn status_helpers() {
        let status = Status::new(0x0000);
        assert!(status.is_success());
        assert!(!status.is_pending());
        let status = Status::with_comment(0xC000, "boom");
        assert_eq!(status.comment.as_deref(), Some("boom"));
        assert_eq!(status.state, StatusState::Failure);
    }

    #[test]
    fn missing_command_field_is_an_error() {
        let ds = Dataset::new();
        match Command::from_dataset(&ds) {
            Err(NetworkError::Protocol(message)) => assert!(message.contains("CommandField"), "got {message}"),
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_command_field_is_an_error() {
        let mut ds = Dataset::new();
        ds.insert_value(tag::COMMAND_FIELD, 0x0099u16);
        assert!(matches!(Command::from_dataset(&ds), Err(NetworkError::Protocol(_))));
    }

    #[test]
    fn move_and_suboperation_fields_survive_a_round_trip() {
        let mut request = Command::request(CommandField::CMoveRq, 5);
        request.move_destination = Some("DEST-AE".into());
        request.affected_sop_instance_uid = Some("1.2.3.4".into());
        request.remaining_suboperations = Some(3);
        request.completed_suboperations = Some(1);
        request.failed_suboperations = Some(0);
        request.warning_suboperations = Some(2);
        request.move_originator_ae_title = Some("ORIGIN".into());
        request.move_originator_message_id = Some(99);

        let back = Command::from_dataset(&request.to_dataset()).unwrap();
        assert_eq!(back.move_destination.as_deref(), Some("DEST-AE"));
        assert_eq!(back.affected_sop_instance_uid.as_deref(), Some("1.2.3.4"));
        assert_eq!(back.remaining_suboperations, Some(3));
        assert_eq!(back.completed_suboperations, Some(1));
        assert_eq!(back.failed_suboperations, Some(0));
        assert_eq!(back.warning_suboperations, Some(2));
        assert_eq!(back.move_originator_ae_title.as_deref(), Some("ORIGIN"));
        assert_eq!(back.move_originator_message_id, Some(99));
    }

    #[test]
    fn n_service_fields_survive_a_round_trip() {
        let mut request = Command::request(CommandField::NActionRq, 3);
        request.requested_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.88.1".into());
        request.requested_sop_instance_uid = Some("1.2.3".into());
        request.event_type_id = Some(2);
        request.action_type_id = Some(1);

        let back = Command::from_dataset(&request.to_dataset()).unwrap();
        assert_eq!(back.requested_sop_class_uid.as_deref(), Some("1.2.840.10008.5.1.4.1.1.88.1"));
        assert_eq!(back.requested_sop_instance_uid.as_deref(), Some("1.2.3"));
        assert_eq!(back.event_type_id, Some(2));
        assert_eq!(back.action_type_id, Some(1));
    }
}
