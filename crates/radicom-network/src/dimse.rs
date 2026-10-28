//! DIMSE command model and status codes (DIMSE-01; docs/01-architecture.md §11).
//!
//! Commands always travel as Implicit VR Little Endian datasets. This module
//! builds and parses the command set; the dataset that follows a command is
//! handled by the protocol engine.

use crate::error::{NetworkError, Result};
use radicom::model::buffer::MemoryBuffer;
use radicom::model::dataset::Dataset;
use radicom::model::element::DicomElement;
use radicom::model::item::DicomItem;
use radicom::model::tag::Tag;
use radicom::model::vr::Vr;
use std::sync::Arc;

/// Command set tags (§6.1).
pub mod tag {
    use radicom::model::tag::Tag;

    pub const COMMAND_GROUP_LENGTH: Tag = Tag::COMMAND_GROUP_LENGTH;
    pub const AFFECTED_SOP_CLASS_UID: Tag = Tag::AFFECTED_SOP_CLASS_UID;
    pub const REQUESTED_SOP_CLASS_UID: Tag = Tag::REQUESTED_SOP_CLASS_UID;
    pub const COMMAND_FIELD: Tag = Tag::COMMAND_FIELD;
    pub const MESSAGE_ID: Tag = Tag::MESSAGE_ID;
    pub const MESSAGE_ID_BEING_RESPONDED_TO: Tag = Tag::MESSAGE_ID_BEING_RESPONDED_TO;
    pub const MOVE_DESTINATION: Tag = Tag::MOVE_DESTINATION;
    pub const PRIORITY: Tag = Tag::PRIORITY;
    pub const COMMAND_DATA_SET_TYPE: Tag = Tag::COMMAND_DATA_SET_TYPE;
    pub const STATUS: Tag = Tag::STATUS;
    pub const ERROR_COMMENT: Tag = Tag::ERROR_COMMENT;
    pub const AFFECTED_SOP_INSTANCE_UID: Tag = Tag::AFFECTED_SOP_INSTANCE_UID;
    pub const REQUESTED_SOP_INSTANCE_UID: Tag = Tag::REQUESTED_SOP_INSTANCE_UID;
    pub const EVENT_TYPE_ID: Tag = Tag::EVENT_TYPE_ID;
    pub const ATTRIBUTE_IDENTIFIER_LIST: Tag = Tag::ATTRIBUTE_IDENTIFIER_LIST;
    pub const ACTION_TYPE_ID: Tag = Tag::ACTION_TYPE_ID;
    pub const NUMBER_OF_REMAINING_SUBOPERATIONS: Tag = Tag::NUMBER_OF_REMAINING_SUBOPERATIONS;
    pub const NUMBER_OF_COMPLETED_SUBOPERATIONS: Tag = Tag::NUMBER_OF_COMPLETED_SUBOPERATIONS;
    pub const NUMBER_OF_FAILED_SUBOPERATIONS: Tag = Tag::NUMBER_OF_FAILED_SUBOPERATIONS;
    pub const NUMBER_OF_WARNING_SUBOPERATIONS: Tag = Tag::NUMBER_OF_WARNING_SUBOPERATIONS;
    pub const MOVE_ORIGINATOR_APPLICATION_ENTITY_TITLE: Tag = Tag::MOVE_ORIGINATOR_APPLICATION_ENTITY_TITLE;
    pub const MOVE_ORIGINATOR_MESSAGE_ID: Tag = Tag::MOVE_ORIGINATOR_MESSAGE_ID;
}

/// `(0000,0800)` value meaning "no dataset follows" (§5.2).
pub const NO_DATA_SET: u16 = 0x0101;

/// Priority element values (PS3.7 Table E.1-1): `MEDIUM = 0`, `HIGH = 1`,
/// `LOW = 2`. Declaration order follows the numeric order, so the derived `Ord`
/// means "medium before high before low".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Priority {
    #[default]
    Medium,
    High,
    Low,
}

impl Priority {
    pub fn code(self) -> u16 {
        match self {
            Priority::Medium => 0,
            Priority::High => 1,
            Priority::Low => 2,
        }
    }

    /// Parses the `(0000,0700)` value; MEDIUM (0) is also the standard default
    /// when the element is absent or unknown.
    pub fn from_code(code: u16) -> Self {
        match code {
            1 => Priority::High,
            2 => Priority::Low,
            _ => Priority::Medium,
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

    /// True when `(0000,0110) Message ID` belongs in this command set.
    ///
    /// PS3.7 Table E.1-1 lists Message ID for requests only: responses carry
    /// `(0000,0120) Message ID Being Responded To` instead, and C-CANCEL names
    /// the request it cancels with `(0000,0120)` too.
    pub fn writes_message_id(self) -> bool {
        self.is_request() && self != CommandField::CCancelRq
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
    /// `(0000,1005)` Attribute Identifier List, used by N-GET requests.
    pub attribute_identifier_list: Option<Vec<Tag>>,
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
            attribute_identifier_list: None,
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
    ///
    /// `None` when `request` has no response counterpart (a response field, or
    /// C-CANCEL): answering it is meaningless, and the caller decides between
    /// aborting the association and dropping the message.
    pub fn response(request: &Command, status: u16) -> Option<Self> {
        let field = response_field(request.field)?;
        let mut out = Command::request(field, request.message_id);
        out.message_id_being_responded_to = Some(request.message_id);
        out.status = Some(status);
        out.affected_sop_class_uid = request.affected_sop_class_uid.clone();
        out.no_dataset = true;
        Some(out)
    }

    /// Builds the command dataset, recomputing `(0000,0000)` (§5.3).
    pub fn to_dataset(&self) -> Dataset {
        let mut ds = Dataset::new();
        // required ordering: group length first, then ascending tags
        insert_us(&mut ds, tag::COMMAND_FIELD, self.field.code());
        if let Some(uid) = &self.affected_sop_class_uid {
            ds.insert_str(tag::AFFECTED_SOP_CLASS_UID, Vr::Ui, uid);
        }
        if let Some(uid) = &self.requested_sop_class_uid {
            ds.insert_str(tag::REQUESTED_SOP_CLASS_UID, Vr::Ui, uid);
        }
        if self.field.writes_message_id() {
            insert_us(&mut ds, tag::MESSAGE_ID, self.message_id);
        }
        if let Some(id) = self.message_id_being_responded_to {
            insert_us(&mut ds, tag::MESSAGE_ID_BEING_RESPONDED_TO, id);
        }
        if let Some(status) = self.status {
            insert_us(&mut ds, tag::STATUS, status);
        }
        if let Some(comment) = &self.error_comment {
            // LO is limited to 64 bytes
            ds.insert_str(tag::ERROR_COMMENT, Vr::Lo, truncate_lo(comment));
        }
        insert_us(&mut ds, tag::PRIORITY, self.priority.code());
        if self.no_dataset {
            insert_us(&mut ds, tag::COMMAND_DATA_SET_TYPE, NO_DATA_SET);
        } else {
            // 0x0001 = "dataset follows, not UID-addressed"
            insert_us(&mut ds, tag::COMMAND_DATA_SET_TYPE, 0x0001u16);
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
        if let Some(tags) = &self.attribute_identifier_list {
            let mut bytes = Vec::with_capacity(tags.len() * 4);
            for tag in tags {
                bytes.extend_from_slice(&tag.group().to_le_bytes());
                bytes.extend_from_slice(&tag.element().to_le_bytes());
            }
            ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
                tag::ATTRIBUTE_IDENTIFIER_LIST,
                Vr::At,
                Arc::new(MemoryBuffer::from(bytes)),
            )));
        }
        if let Some(id) = self.event_type_id {
            insert_us(&mut ds, tag::EVENT_TYPE_ID, id);
        }
        if let Some(id) = self.action_type_id {
            insert_us(&mut ds, tag::ACTION_TYPE_ID, id);
        }
        if let Some(ae) = &self.move_originator_ae_title {
            ds.insert_str(tag::MOVE_ORIGINATOR_APPLICATION_ENTITY_TITLE, Vr::Ae, ae);
        }
        if let Some(id) = self.move_originator_message_id {
            insert_us(&mut ds, tag::MOVE_ORIGINATOR_MESSAGE_ID, id);
        }
        if let Some(n) = self.remaining_suboperations {
            insert_us(&mut ds, tag::NUMBER_OF_REMAINING_SUBOPERATIONS, n);
        }
        if let Some(n) = self.completed_suboperations {
            insert_us(&mut ds, tag::NUMBER_OF_COMPLETED_SUBOPERATIONS, n);
        }
        if let Some(n) = self.failed_suboperations {
            insert_us(&mut ds, tag::NUMBER_OF_FAILED_SUBOPERATIONS, n);
        }
        if let Some(n) = self.warning_suboperations {
            insert_us(&mut ds, tag::NUMBER_OF_WARNING_SUBOPERATIONS, n);
        }

        // (0000,0000) is recomputed by the writer when it encodes the command
        // set with `keep_group_lengths`; the placeholder keeps tag order right
        // and makes the dataset self-describing for tests.
        let mut with_len = ds.clone();
        insert_ul(&mut with_len, tag::COMMAND_GROUP_LENGTH, 0);
        let total = radicom::io::writer::DicomWriter::default()
            .dataset_length(&with_len, &radicom::model::transfer_syntax::syntaxes::IMPLICIT_VR_LITTLE_ENDIAN)
            .unwrap_or(0);
        // the group length element itself is 12 bytes (tag + length + value)
        insert_ul(&mut ds, tag::COMMAND_GROUP_LENGTH, (total.saturating_sub(12)) as u32);
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
        let attribute_identifier_list = if ds.contains(&tag::ATTRIBUTE_IDENTIFIER_LIST) {
            let element = ds
                .get_element(tag::ATTRIBUTE_IDENTIFIER_LIST)
                .map_err(|_| NetworkError::protocol("Attribute Identifier List is not an element"))?;
            let Some(buffer) = element.value().as_buffer() else {
                return Err(NetworkError::protocol("Attribute Identifier List is not binary"));
            };
            let bytes = buffer.data();
            if bytes.len() % 4 != 0 {
                return Err(NetworkError::protocol("Attribute Identifier List has an invalid length"));
            }
            let tags = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|chunk| Tag::new(
                    u16::from_le_bytes([chunk[0], chunk[1]]),
                    u16::from_le_bytes([chunk[2], chunk[3]]),
                ))
                .collect();
            Some(tags)
        } else {
            None
        };

        Ok(Command {
            field,
            affected_sop_class_uid: ds.try_get_str(tag::AFFECTED_SOP_CLASS_UID).map(|s| s.trim().to_string()),
            requested_sop_class_uid: ds.try_get_str(tag::REQUESTED_SOP_CLASS_UID).map(|s| s.trim().to_string()),
            message_id: ds.get_or(tag::MESSAGE_ID, 0u16),
            message_id_being_responded_to: ds.get(tag::MESSAGE_ID_BEING_RESPONDED_TO).ok(),
            status: ds.get(tag::STATUS).ok(),
            error_comment: ds.try_get_str(tag::ERROR_COMMENT).map(|s| s.trim_end().to_string()),
            priority: Priority::from_code(ds.get_or(tag::PRIORITY, 0u16)),
            no_dataset: data_set_type == NO_DATA_SET,
            move_destination: ds.try_get_str(tag::MOVE_DESTINATION).map(|s| s.trim().to_string()),
            affected_sop_instance_uid: ds.try_get_str(tag::AFFECTED_SOP_INSTANCE_UID).map(|s| s.trim().to_string()),
            requested_sop_instance_uid: ds.try_get_str(tag::REQUESTED_SOP_INSTANCE_UID).map(|s| s.trim().to_string()),
            attribute_identifier_list,
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

/// Inserts a `u16` as a binary US value.
///
/// `Dataset::insert_value` maps integers through their string preference
/// (`Vr::Is`), which is right for ordinary datasets but wrong for the command
/// set, where PS3.5 §7.7 fixes every field's VR.
fn insert_us(ds: &mut Dataset, tag: Tag, value: u16) -> &mut Dataset {
    ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
        tag,
        Vr::Us,
        Arc::new(MemoryBuffer::from(value.to_le_bytes().to_vec())),
    )))
}

/// Inserts a `u32` as a binary UL value.
fn insert_ul(ds: &mut Dataset, tag: Tag, value: u32) -> &mut Dataset {
    ds.insert_or_update(DicomItem::Element(DicomElement::new_bytes(
        tag,
        Vr::Ul,
        Arc::new(MemoryBuffer::from(value.to_le_bytes().to_vec())),
    )))
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

    /// Returns the standard generic description for this status code.
    ///
    /// Some DICOM service classes assign different wording to the same code
    /// (for example `A900`); callers that need that distinction should keep
    /// the service class alongside the numeric code.
    pub fn description(&self) -> &'static str {
        status_description(self.code)
    }
}

/// Returns a generic PS3.7/PS3.4 description for a DIMSE status code.
pub fn status_description(code: u16) -> &'static str {
    match code {
        0x0000 => "Success",
        0x0001 | 0x0002 | 0x0107 | 0x0108 => "Warning",
        0x0100..=0x0106 | 0xFF00 => "Pending",
        0xFF01 => "Pending with warning",
        0xFE00 => "Cancel",
        0x0110 => "Processing failure",
        0x0122 => "SOP class not supported",
        0xA700 => "Out of resources",
        0xA701 => "Unable to calculate number of matches",
        0xA702 => "Unable to perform sub-operations",
        0xA801 => "Move destination unknown",
        0xA900 => "Identifier or data set does not match SOP class",
        0xB000..=0xBFFF => "Completed with warnings",
        0xC000..=0xCFFF => "Unable to process",
        _ => "Unknown status",
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

/// The generic status codes (§6.2 / PS3.7 Annex C), usable as constants.
///
/// A few numeric codes carry a service-class-specific name: `0xA900` is
/// "Identifier does not match SOP Class" in Query/Retrieve and "Data Set does
/// not match SOP Class" in Storage, and `0xA700` is "Refused: Out of resources"
/// in both. Those shared values are kept as separate, correctly named constants
/// rather than collapsed into an opaque alias.
pub mod status {
    pub const SUCCESS: u16 = 0x0000;
    pub const CANCEL: u16 = 0xFE00;
    pub const PENDING: u16 = 0xFF00;
    pub const PENDING_WARNING: u16 = 0xFF01;
    pub const PROCESSING_FAILURE: u16 = 0x0110;
    pub const SOP_CLASS_NOT_SUPPORTED: u16 = 0x0122;
    pub const OUT_OF_RESOURCES: u16 = 0xA700;
    pub const UNABLE_TO_PROCESS: u16 = 0xC000;
    /// Query/Retrieve: "Identifier does not match SOP Class" (PS3.7 Annex C).
    pub const IDENTIFIER_DOES_NOT_MATCH_SOP_CLASS: u16 = 0xA900;
    // storage
    pub const STORAGE_FAILURE: u16 = 0xA700;
    pub const STORAGE_OUT_OF_RESOURCES: u16 = 0xA700;
    /// Storage: "Data Set does not match SOP Class" (PS3.7 Annex C).
    pub const STORAGE_DATA_SET_DOES_NOT_MATCH_SOP_CLASS: u16 = 0xA900;
    // query/retrieve
    pub const REFUSED_OUT_OF_RESOURCES: u16 = 0xA700;
    pub const REFUSED_SOP_CLASS_NOT_SUPPORTED: u16 = 0x0122;
    pub const CANCELLED: u16 = 0xFE00;
    pub const MOVE_DESTINATION_UNKNOWN: u16 = 0xA801;
    pub const SUBOPERATIONS_COMPLETED_WITH_WARNINGS: u16 = 0xB000;
    /// No sub-operation succeeded (PS3.4 C-GET/C-MOVE terminal failure).
    pub const UNABLE_TO_PERFORM_SUBOPERATIONS: u16 = 0xA702;
    /// C-GET: "Refused: Out of Resources - Unable to calculate number of
    /// matches" (PS3.7 Annex C / PS3.4 C.4.3). Previously the opaque
    /// `OUT_OF_RESOURCES_2`, which named neither the service nor the meaning.
    pub const UNABLE_TO_CALCULATE_MATCH_COUNT: u16 = 0xA701;
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
    fn priority_codes_follow_ps3_7() {
        // PS3.7 Table E.1-1: MEDIUM = 0, HIGH = 1, LOW = 2 — not the
        // intuitive order, and MEDIUM is the default.
        assert_eq!(Priority::Medium.code(), 0);
        assert_eq!(Priority::High.code(), 1);
        assert_eq!(Priority::Low.code(), 2);
        assert_eq!(Priority::from_code(0), Priority::Medium);
        assert_eq!(Priority::from_code(1), Priority::High);
        assert_eq!(Priority::from_code(2), Priority::Low);
        assert_eq!(Priority::from_code(7), Priority::Medium, "an unknown value reads as the default");
        assert_eq!(Priority::default(), Priority::Medium);
        assert!(Priority::Medium < Priority::High && Priority::High < Priority::Low, "Ord follows the codes");
    }

    #[test]
    fn priority_lands_in_0000_0700_with_the_standard_value() {
        use crate::test_wire::{command_bytes, element_value};

        for (priority, code) in [(Priority::Medium, 0u16), (Priority::High, 1), (Priority::Low, 2)] {
            let mut command = Command::request(CommandField::CFindRq, 1);
            command.priority = priority;
            let bytes = command_bytes(&command);
            assert_eq!(
                element_value(&bytes, 0x0000, 0x0700),
                Some(code.to_le_bytes().to_vec()),
                "{priority:?} must encode as {code} in (0000,0700)"
            );
        }

        // an absent (0000,0700) reads as MEDIUM
        let mut ds = Dataset::new();
        ds.insert_value(tag::COMMAND_FIELD, CommandField::CFindRq.code());
        assert_eq!(Command::from_dataset(&ds).unwrap().priority, Priority::Medium);
    }

    #[test]
    fn responses_do_not_write_message_id() {
        use crate::test_wire::{command_bytes, element_value};

        // PS3.7 Table E.1-1: Message ID (0000,0110) is a request element;
        // responses name the request in (0000,0120).
        let request = Command::request(CommandField::CEchoRq, 7);
        let bytes = command_bytes(&request);
        assert_eq!(element_value(&bytes, 0x0000, 0x0110), Some(7u16.to_le_bytes().to_vec()), "a request keeps it");

        let response = Command::response(&request, status::SUCCESS).expect("C-ECHO has a response");
        let bytes = command_bytes(&response);
        assert_eq!(element_value(&bytes, 0x0000, 0x0110), None, "a response must not carry Message ID");
        assert_eq!(element_value(&bytes, 0x0000, 0x0120), Some(7u16.to_le_bytes().to_vec()));

        // C-CANCEL identifies the cancelled request in (0000,0120) only
        let mut cancel = Command::request(CommandField::CCancelRq, 7);
        cancel.message_id_being_responded_to = Some(7);
        let bytes = command_bytes(&cancel);
        assert_eq!(element_value(&bytes, 0x0000, 0x0110), None, "C-CANCEL has no Message ID");
        assert_eq!(element_value(&bytes, 0x0000, 0x0120), Some(7u16.to_le_bytes().to_vec()));
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
        let response = Command::response(&request, status::PENDING).expect("C-FIND has a response");
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
    fn response_is_none_without_a_counterpart() {
        // a peer can send any of these as a "request": none may panic, and
        // none has a meaningful response
        for field in [
            CommandField::CStoreRsp,
            CommandField::CFindRsp,
            CommandField::CEchoRsp,
            CommandField::NGetRsp,
            CommandField::CCancelRq,
        ] {
            let request = Command::request(field, 1);
            assert_eq!(Command::response(&request, status::SUCCESS), None, "{field:?}");
        }
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
    fn status_constants_match_ps37_annex_c_literals() {
        // Every constant is spelled out against its PS3.7 Annex C / PS3.4 value
        // with a literal, so a changed or mistyped constant fails here rather
        // than silently agreeing with itself.
        use status::*;
        assert_eq!(SUCCESS, 0x0000);
        assert_eq!(CANCEL, 0xFE00);
        assert_eq!(CANCELLED, 0xFE00);
        assert_eq!(PENDING, 0xFF00);
        assert_eq!(PENDING_WARNING, 0xFF01);
        assert_eq!(PROCESSING_FAILURE, 0x0110);
        assert_eq!(SOP_CLASS_NOT_SUPPORTED, 0x0122);
        assert_eq!(REFUSED_SOP_CLASS_NOT_SUPPORTED, 0x0122);
        assert_eq!(UNABLE_TO_PROCESS, 0xC000);
        // 0xA700: Refused - Out of resources (Storage and Query/Retrieve alike)
        assert_eq!(OUT_OF_RESOURCES, 0xA700);
        assert_eq!(STORAGE_OUT_OF_RESOURCES, 0xA700);
        assert_eq!(STORAGE_FAILURE, 0xA700);
        assert_eq!(REFUSED_OUT_OF_RESOURCES, 0xA700);
        // C-GET: out of resources - unable to calculate matches / sub-operations
        assert_eq!(UNABLE_TO_CALCULATE_MATCH_COUNT, 0xA701);
        assert_eq!(UNABLE_TO_PERFORM_SUBOPERATIONS, 0xA702);
        assert_eq!(MOVE_DESTINATION_UNKNOWN, 0xA801);
        // 0xA900: does not match SOP Class (Q/R Identifier, Storage Data Set)
        assert_eq!(IDENTIFIER_DOES_NOT_MATCH_SOP_CLASS, 0xA900);
        assert_eq!(STORAGE_DATA_SET_DOES_NOT_MATCH_SOP_CLASS, 0xA900);
        assert_eq!(SUBOPERATIONS_COMPLETED_WITH_WARNINGS, 0xB000);
    }

    #[test]
    fn status_helpers() {
        let status = Status::new(0x0000);
        assert!(status.is_success());
        assert!(!status.is_pending());
        assert_eq!(status.description(), "Success");
        let status = Status::with_comment(0xC000, "boom");
        assert_eq!(status.comment.as_deref(), Some("boom"));
        assert_eq!(status.state, StatusState::Failure);
        assert_eq!(status.description(), "Unable to process");
        assert_eq!(Status::new(0xB123).description(), "Completed with warnings");
        assert_eq!(Status::new(0xDEAD).description(), "Unknown status");
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
