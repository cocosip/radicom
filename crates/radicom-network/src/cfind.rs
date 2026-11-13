//! C-FIND (SRV-04; docs/architecture.md §11).
//!
//! A C-FIND carries an Identifier dataset (the query keys) and is answered by
//! zero or more Pending responses, each with its own Identifier, followed by
//! exactly one terminal response.
//!
//! The SCP side returns a stream so the engine can send items as they are
//! produced instead of buffering the whole result set.

use crate::dimse::{status, Command, CommandField, StatusState};
use crate::error::{NetworkError, Result};
use radicom::model::dataset::Dataset;
use radicom::model::tag::Tag;
use radicom::model::vr::Vr;

/// Query/Retrieve level (0008,0052).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryRetrieveLevel {
    Patient,
    Study,
    Series,
    Image,
    /// Modality worklist (not a Q/R level, but the same mechanism).
    Worklist,
}

impl QueryRetrieveLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            QueryRetrieveLevel::Patient => "PATIENT",
            QueryRetrieveLevel::Study => "STUDY",
            QueryRetrieveLevel::Series => "SERIES",
            QueryRetrieveLevel::Image => "IMAGE",
            QueryRetrieveLevel::Worklist => "WORKLIST",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_uppercase().as_str() {
            "PATIENT" => Some(QueryRetrieveLevel::Patient),
            "STUDY" => Some(QueryRetrieveLevel::Study),
            "SERIES" => Some(QueryRetrieveLevel::Series),
            "IMAGE" => Some(QueryRetrieveLevel::Image),
            "WORKLIST" => Some(QueryRetrieveLevel::Worklist),
            _ => None,
        }
    }

    /// The SOP class this level queries under (§11.4).
    pub fn sop_class(self, root: QueryRoot) -> &'static str {
        use crate::association::sop_class as sc;
        match (root, self) {
            (_, QueryRetrieveLevel::Worklist) => sc::MODALITY_WORKLIST_FIND,
            (QueryRoot::Patient, _) => sc::PATIENT_ROOT_QR_FIND,
            (QueryRoot::Study, _) => sc::STUDY_ROOT_QR_FIND,
        }
    }

    /// The retrieve SOP class for this level (§11.4).
    ///
    /// A worklist is query-only, so it has no retrieve model.
    pub fn retrieve_sop_class(
        self,
        root: QueryRoot,
        service: RetrieveService,
    ) -> Option<&'static str> {
        use crate::association::sop_class as sc;
        if self == QueryRetrieveLevel::Worklist {
            return None;
        }
        let pair = match (service, root) {
            (RetrieveService::Get, QueryRoot::Patient) => sc::PATIENT_ROOT_QR_GET,
            (RetrieveService::Get, QueryRoot::Study) => sc::STUDY_ROOT_QR_GET,
            (RetrieveService::Move, QueryRoot::Patient) => sc::PATIENT_ROOT_QR_MOVE,
            (RetrieveService::Move, QueryRoot::Study) => sc::STUDY_ROOT_QR_MOVE,
        };
        Some(pair)
    }

    /// The C-MOVE SOP class for this level (§11.4).
    pub fn move_sop_class(self, root: QueryRoot) -> Option<&'static str> {
        self.retrieve_sop_class(root, RetrieveService::Move)
    }

    /// The C-GET SOP class for this level (§11.4).
    pub fn get_sop_class(self, root: QueryRoot) -> Option<&'static str> {
        self.retrieve_sop_class(root, RetrieveService::Get)
    }
}

/// Which Q/R information model the query uses (§11.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryRoot {
    Patient,
    #[default]
    Study,
}

/// The retrieve service a request belongs to (§11.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrieveService {
    /// C-GET: instances come back on the *same* association.
    Get,
    /// C-MOVE: the SCP pushes instances to a third party.
    Move,
}

/// Return keys that identify the entity at each level, in tag order.
///
/// These are the mandatory keys a well-formed query returns; callers add more.
const STUDY_KEYS: &[(u16, u16, Vr)] = &[
    (0x0008, 0x0020, Vr::Da), // Study Date
    (0x0008, 0x0030, Vr::Tm), // Study Time
    (0x0008, 0x0050, Vr::Sh), // Accession Number
    (0x0008, 0x0061, Vr::Cs), // Modalities in Study
    (0x0008, 0x0090, Vr::Pn), // Referring Physician's Name
    (0x0010, 0x0010, Vr::Pn), // Patient's Name
    (0x0010, 0x0020, Vr::Lo), // Patient ID
    (0x0020, 0x000D, Vr::Ui), // Study Instance UID
    (0x0020, 0x0010, Vr::Sh), // Study ID
];

const SERIES_KEYS: &[(u16, u16, Vr)] = &[
    (0x0008, 0x0060, Vr::Cs), // Modality
    (0x0020, 0x000E, Vr::Ui), // Series Instance UID
    (0x0020, 0x0011, Vr::Is), // Series Number
];

const IMAGE_KEYS: &[(u16, u16, Vr)] = &[
    (0x0008, 0x0018, Vr::Ui), // SOP Instance UID
    (0x0020, 0x0013, Vr::Is), // Instance Number
];

const PATIENT_KEYS: &[(u16, u16, Vr)] = &[(0x0010, 0x0010, Vr::Pn)];

/// Return keys inside the Scheduled Procedure Step Sequence of a Modality
/// Worklist query (PS3.4 §K.6.1.2.2, Table K.6-1).
const WORKLIST_STEP_KEYS: &[(u16, u16, Vr)] = &[
    (0x0040, 0x0001, Vr::Ae), // Scheduled Station AE Title
    (0x0040, 0x0002, Vr::Da), // Scheduled Procedure Step Start Date
    (0x0040, 0x0003, Vr::Tm), // Scheduled Procedure Step Start Time
    (0x0040, 0x0006, Vr::Pn), // Scheduled Performing Physician's Name
    (0x0040, 0x0007, Vr::Lo), // Scheduled Procedure Step Description
    (0x0040, 0x0009, Vr::Sh), // Scheduled Procedure Step ID
    (0x0040, 0x0011, Vr::Sh), // Scheduled Procedure Step Location
];

/// Top-level return keys a Modality Worklist item also carries.
const WORKLIST_KEYS: &[(u16, u16, Vr)] = &[
    (0x0008, 0x0050, Vr::Sh), // Accession Number
    (0x0010, 0x0010, Vr::Pn), // Patient's Name
    (0x0010, 0x0020, Vr::Lo), // Patient ID
    (0x0020, 0x000D, Vr::Ui), // Study Instance UID
    (0x0032, 0x1060, Vr::Lo), // Requested Procedure Description
    (0x0040, 0x1001, Vr::Sh), // Requested Procedure ID
];

/// A C-FIND request (§11.4).
#[derive(Debug, Clone)]
pub struct CFindRequest {
    /// Message ID; zero asks high-level send APIs to allocate one.
    pub message_id: u16,
    /// The abstract syntax the query runs under, derived from level + root.
    pub sop_class_uid: String,
    pub priority: crate::dimse::Priority,
    /// The Identifier: matching keys with values, return keys empty.
    pub identifier: Dataset,
}

impl CFindRequest {
    /// Builds a query for `level` with the standard return keys.
    pub fn new(level: QueryRetrieveLevel, root: QueryRoot, message_id: u16) -> Self {
        let mut identifier = Dataset::new();
        if level == QueryRetrieveLevel::Worklist {
            // Modality Worklist has no Query/Retrieve Level: its keys live in
            // the Scheduled Procedure Step Sequence (PS3.4 §K.6.1.2.2), and a
            // strict MWL SCP answers 0xA900 to a stray (0008,0052).
            let mut step = Dataset::new();
            for (group, element, vr) in WORKLIST_STEP_KEYS {
                step.insert_empty(Tag::new(*group, *element), *vr);
            }
            let sequence = identifier.insert_sequence(Tag::SCHEDULED_PROCEDURE_STEP_SEQUENCE);
            sequence.push(step);
            for (group, element, vr) in WORKLIST_KEYS {
                identifier.insert_empty(Tag::new(*group, *element), *vr);
            }
        } else {
            identifier.insert_str(Tag::QUERY_RETRIEVE_LEVEL, Vr::Cs, level.as_str());
            for (group, element, vr) in level_keys(level) {
                identifier.insert_empty(Tag::new(*group, *element), *vr);
            }
        }
        CFindRequest {
            message_id,
            sop_class_uid: level.sop_class(root).to_string(),
            priority: crate::dimse::Priority::Medium,
            identifier,
        }
    }

    /// A patient-level query.
    pub fn patient_query(message_id: u16) -> Self {
        CFindRequest::new(QueryRetrieveLevel::Patient, QueryRoot::Patient, message_id)
    }

    /// A study-level query under the Study Root model.
    pub fn study_query(message_id: u16) -> Self {
        CFindRequest::new(QueryRetrieveLevel::Study, QueryRoot::Study, message_id)
    }

    /// A series-level query under the Study Root model.
    pub fn series_query(message_id: u16) -> Self {
        CFindRequest::new(QueryRetrieveLevel::Series, QueryRoot::Study, message_id)
    }

    /// An image-level query under the Study Root model.
    pub fn image_query(message_id: u16) -> Self {
        CFindRequest::new(QueryRetrieveLevel::Image, QueryRoot::Study, message_id)
    }

    /// A modality worklist query.
    pub fn worklist_query(message_id: u16) -> Self {
        CFindRequest::new(QueryRetrieveLevel::Worklist, QueryRoot::Study, message_id)
    }

    /// Sets a matching key to `value` (creates the element if absent).
    ///
    /// Prefer [`Self::with_tag`]: a raw `(group, element)` pair bypasses the
    /// dictionary and is the pattern docs/architecture.md §2.1 forbids.
    pub fn with_key(mut self, group: u16, element: u16, vr: Vr, value: impl Into<String>) -> Self {
        self.identifier
            .insert_str(Tag::new(group, element), vr, value);
        self
    }

    /// Sets a matching key by [`Tag`] (the dictionary-typed form of
    /// [`Self::with_key`]).
    pub fn with_tag(mut self, tag: Tag, vr: Vr, value: impl Into<String>) -> Self {
        self.identifier.insert_str(tag, vr, value);
        self
    }

    /// The query level, when the Identifier states one.
    pub fn level(&self) -> Option<QueryRetrieveLevel> {
        if let Some(level) = self
            .identifier
            .try_get_str(Tag::QUERY_RETRIEVE_LEVEL)
            .and_then(|s| QueryRetrieveLevel::parse(&s))
        {
            return Some(level);
        }
        // MWL omits (0008,0052); the SPS sequence is its signature
        self.identifier
            .contains(&Tag::SCHEDULED_PROCEDURE_STEP_SEQUENCE)
            .then_some(QueryRetrieveLevel::Worklist)
    }

    /// The command for this request; the Identifier travels as the dataset.
    pub fn to_command(&self) -> Command {
        let mut command = Command::request(CommandField::CFindRq, self.message_id);
        command.affected_sop_class_uid = Some(self.sop_class_uid.clone());
        command.priority = self.priority;
        command.no_dataset = false;
        command
    }
}

/// The standard return keys for a level.
fn level_keys(level: QueryRetrieveLevel) -> &'static [(u16, u16, Vr)] {
    match level {
        QueryRetrieveLevel::Patient => PATIENT_KEYS,
        QueryRetrieveLevel::Study => STUDY_KEYS,
        QueryRetrieveLevel::Series => SERIES_KEYS,
        QueryRetrieveLevel::Image => IMAGE_KEYS,
        // worklist has its own shape; `new` builds it separately
        QueryRetrieveLevel::Worklist => WORKLIST_KEYS,
    }
}

/// A C-FIND response (§11.4).
#[derive(Debug, Clone)]
pub struct CFindResponse {
    pub message_id_being_responded_to: u16,
    /// The abstract syntax echoed in optional `(0000,0002)`; empty means absent
    /// (PS3.7 Table 9.3-4).
    pub affected_sop_class_uid: String,
    pub status: u16,
    /// Present on Pending responses: one matching item.
    pub identifier: Option<Dataset>,
    /// Local bookkeeping only: a C-FIND-RSP carries no sub-operation counters
    /// (PS3.7 Table 9.3-4 lists none), so these are never written to the wire.
    pub remaining: u16,
    pub completed: u16,
    pub failed: u16,
    pub warning: u16,
    pub error_comment: Option<String>,
}

impl CFindResponse {
    /// A Pending response carrying one match.
    pub fn pending(request: &CFindRequest, identifier: Dataset) -> Self {
        CFindResponse {
            message_id_being_responded_to: request.message_id,
            affected_sop_class_uid: request.sop_class_uid.clone(),
            status: status::PENDING,
            identifier: Some(identifier),
            remaining: 0,
            completed: 0,
            failed: 0,
            warning: 0,
            error_comment: None,
        }
    }

    /// The terminal success response, with the match count reported as completed.
    pub fn success(request: &CFindRequest, matches: u16) -> Self {
        CFindResponse {
            message_id_being_responded_to: request.message_id,
            affected_sop_class_uid: request.sop_class_uid.clone(),
            status: status::SUCCESS,
            identifier: None,
            remaining: 0,
            completed: matches,
            failed: 0,
            warning: 0,
            error_comment: None,
        }
    }

    /// A terminal failure response.
    pub fn failure(request: &CFindRequest, code: u16, comment: impl Into<String>) -> Self {
        CFindResponse {
            message_id_being_responded_to: request.message_id,
            affected_sop_class_uid: request.sop_class_uid.clone(),
            status: code,
            identifier: None,
            remaining: 0,
            completed: 0,
            failed: 0,
            warning: 0,
            error_comment: Some(crate::dimse::truncate_lo(&comment.into())),
        }
    }

    /// True when more responses follow this one.
    pub fn is_pending(&self) -> bool {
        self.state() == StatusState::Pending
    }

    pub fn state(&self) -> StatusState {
        crate::dimse::classify(self.status)
    }

    /// The command for this response; the Identifier travels as the dataset.
    pub fn to_command(&self) -> Command {
        let mut command =
            Command::request(CommandField::CFindRsp, self.message_id_being_responded_to);
        command.message_id_being_responded_to = Some(self.message_id_being_responded_to);
        // PS3.7 Table 9.3-4 makes Affected SOP Class UID optional.
        command.affected_sop_class_uid =
            (!self.affected_sop_class_uid.is_empty()).then(|| self.affected_sop_class_uid.clone());
        command.status = Some(self.status);
        command.no_dataset = self.identifier.is_none();
        command.error_comment = self.error_comment.clone();
        // No `(0000,1020..1023)` sub-operation counters on a C-FIND-RSP: those
        // fields belong to C-MOVE-RSP / C-GET-RSP only (PS3.7 Table 9.3-4).
        command
    }

    pub fn from_command(command: &Command, identifier: Option<Dataset>) -> Result<Self> {
        if command.field != CommandField::CFindRsp {
            return Err(NetworkError::protocol("not a C-FIND response"));
        }
        let status = command.status.unwrap_or(status::PROCESSING_FAILURE);
        let pending = matches!(status, status::PENDING | status::PENDING_WARNING);
        if pending != identifier.is_some() {
            return Err(NetworkError::protocol(
                "C-FIND Pending responses require an Identifier and terminal responses forbid one",
            ));
        }
        let message_id_being_responded_to =
            command
                .message_id_being_responded_to
                .ok_or(NetworkError::protocol(
                    "C-FIND response has no valid Message ID Being Responded To",
                ))?;
        let affected_sop_class_uid = command.affected_sop_class_uid.clone().unwrap_or_default();
        Ok(CFindResponse {
            message_id_being_responded_to,
            affected_sop_class_uid,
            status,
            identifier,
            remaining: command.remaining_suboperations.unwrap_or(0),
            completed: command.completed_suboperations.unwrap_or(0),
            failed: command.failed_suboperations.unwrap_or(0),
            warning: command.warning_suboperations.unwrap_or(0),
            error_comment: command.error_comment.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::association::sop_class;
    use crate::services::ServiceProvider;
    use futures::stream::BoxStream;
    use futures::StreamExt;

    #[test]
    fn a_query_response_without_a_status_is_not_a_success() {
        // PS3.7: Status is mandatory in a response; a peer that omits it must
        // not be read as a completed operation (NET-08). C-FIND, C-GET and
        // C-MOVE share the same parser shape, so all three are checked.
        for (request_field, response_field) in [
            (CommandField::CFindRq, CommandField::CFindRsp),
            (CommandField::CGetRq, CommandField::CGetRsp),
            (CommandField::CMoveRq, CommandField::CMoveRsp),
        ] {
            let mut request = Command::request(request_field, 5);
            request.affected_sop_class_uid = Some(sop_class::STUDY_ROOT_QR_FIND.into());
            let mut response = Command::response(&request, status::SUCCESS).expect("a response");
            assert_eq!(response.field, response_field);
            if matches!(
                response_field,
                CommandField::CGetRsp | CommandField::CMoveRsp
            ) {
                response.completed_suboperations = Some(0);
                response.failed_suboperations = Some(0);
                response.warning_suboperations = Some(0);
            }

            let parsed = match response_field {
                CommandField::CGetRsp => {
                    crate::cget::CGetResponse::from_command(&response)
                        .unwrap()
                        .status
                }
                CommandField::CMoveRsp => {
                    crate::cmove::CMoveResponse::from_command(&response)
                        .unwrap()
                        .status
                }
                _ => CFindResponse::from_command(&response, None).unwrap().status,
            };
            assert_eq!(
                parsed,
                status::SUCCESS,
                "{response_field:?} reads the status it was sent"
            );

            response.status = None;
            match response_field {
                CommandField::CGetRsp => assert!(
                    crate::cget::CGetResponse::from_command(&response).is_err(),
                    "C-GET without Status is malformed"
                ),
                CommandField::CMoveRsp => assert!(
                    crate::cmove::CMoveResponse::from_command(&response).is_err(),
                    "C-MOVE without Status is malformed"
                ),
                _ => assert_eq!(
                    CFindResponse::from_command(&response, None).unwrap().status,
                    status::PROCESSING_FAILURE,
                    "C-FIND without Status must not read as success"
                ),
            }
        }
    }

    #[test]
    fn levels_map_to_the_standard_sop_classes() {
        assert_eq!(
            QueryRetrieveLevel::Study.sop_class(QueryRoot::Study),
            sop_class::STUDY_ROOT_QR_FIND
        );
        assert_eq!(
            QueryRetrieveLevel::Patient.sop_class(QueryRoot::Patient),
            sop_class::PATIENT_ROOT_QR_FIND
        );
        assert_eq!(
            QueryRetrieveLevel::Image.sop_class(QueryRoot::Study),
            sop_class::STUDY_ROOT_QR_FIND
        );
        assert_eq!(
            QueryRetrieveLevel::Worklist.sop_class(QueryRoot::Study),
            sop_class::MODALITY_WORKLIST_FIND
        );
    }

    #[test]
    fn level_strings_round_trip() {
        for level in [
            QueryRetrieveLevel::Patient,
            QueryRetrieveLevel::Study,
            QueryRetrieveLevel::Series,
            QueryRetrieveLevel::Image,
            QueryRetrieveLevel::Worklist,
        ] {
            assert_eq!(QueryRetrieveLevel::parse(level.as_str()), Some(level));
        }
        assert_eq!(QueryRetrieveLevel::parse("nonsense"), None);
        assert_eq!(
            QueryRetrieveLevel::parse(" study "),
            Some(QueryRetrieveLevel::Study),
            "trimmed and case-insensitive"
        );
    }

    #[test]
    fn constructors_set_the_level_and_return_keys() {
        let request = CFindRequest::study_query(7);
        assert_eq!(request.level(), Some(QueryRetrieveLevel::Study));
        assert_eq!(request.sop_class_uid, sop_class::STUDY_ROOT_QR_FIND);
        assert_eq!(request.message_id, 7);
        // the standard study return keys are present and empty
        assert!(
            request.identifier.contains(&Tag::STUDY_INSTANCE_UID),
            "Study Instance UID"
        );
        assert!(
            request.identifier.contains(&Tag::PATIENT_NAME),
            "Patient Name"
        );
        assert_eq!(
            request.identifier.get_str(Tag::PATIENT_NAME).unwrap(),
            "",
            "return keys start empty"
        );

        let worklist = CFindRequest::worklist_query(1);
        assert_eq!(worklist.sop_class_uid, sop_class::MODALITY_WORKLIST_FIND);
        assert_eq!(worklist.level(), Some(QueryRetrieveLevel::Worklist));
        // MWL keys live in the Scheduled Procedure Step Sequence, not in a
        // Query/Retrieve Level (PS3.4 §K.6.1.2.2)
        assert!(
            !worklist.identifier.contains(&Tag::QUERY_RETRIEVE_LEVEL),
            "a worklist query must not carry a Query/Retrieve Level"
        );
        let steps = worklist
            .identifier
            .try_get_sequence(Tag::SCHEDULED_PROCEDURE_STEP_SEQUENCE)
            .expect("worklist has an SPS sequence");
        assert_eq!(steps.len(), 1);
        assert!(steps.items()[0].contains(&Tag::SCHEDULED_STATION_AE_TITLE));
        assert!(steps.items()[0].contains(&Tag::SCHEDULED_PROCEDURE_STEP_START_DATE));
        assert!(worklist.identifier.contains(&Tag::REQUESTED_PROCEDURE_ID));
    }

    #[test]
    fn with_tag_sets_a_key_from_the_dictionary() {
        let request = CFindRequest::study_query(1).with_tag(Tag::PATIENT_ID, Vr::Lo, "PID-9");
        assert_eq!(
            request.identifier.get_str(Tag::PATIENT_ID).unwrap(),
            "PID-9"
        );
    }

    #[test]
    fn matching_keys_are_settable() {
        let request = CFindRequest::study_query(3)
            .with_key(0x0010, 0x0020, Vr::Lo, "PID-1")
            .with_key(0x0008, 0x0061, Vr::Cs, "CT");
        assert_eq!(
            request.identifier.get_str(Tag::PATIENT_ID).unwrap(),
            "PID-1"
        );
        assert_eq!(
            request
                .identifier
                .get_str(Tag::MODALITIES_IN_STUDY)
                .unwrap(),
            "CT"
        );
    }

    #[test]
    fn the_worklist_key_table_is_exposed_for_the_level() {
        // `new` builds a worklist separately, but level_keys must still answer
        // for every level (it is the exhaustive match other callers rely on).
        assert_eq!(
            level_keys(QueryRetrieveLevel::Worklist),
            WORKLIST_KEYS,
            "worklist resolves to its top-level key table"
        );
    }

    #[test]
    fn command_carries_the_identifier() {
        let request = CFindRequest::series_query(11);
        let command = request.to_command();
        assert_eq!(command.field, CommandField::CFindRq);
        assert_eq!(command.message_id, 11);
        assert!(!command.no_dataset, "the Identifier travels as a dataset");
        assert_eq!(
            command.affected_sop_class_uid.as_deref(),
            Some(sop_class::STUDY_ROOT_QR_FIND)
        );
    }

    #[test]
    fn pending_response_carries_one_item() {
        let request = CFindRequest::study_query(2);
        let mut item = Dataset::new();
        item.insert_str(Tag::STUDY_INSTANCE_UID, Vr::Ui, "1.2.3");
        let response = CFindResponse::pending(&request, item);
        assert!(response.is_pending());
        assert_eq!(response.state(), StatusState::Pending);
        assert!(response.identifier.is_some());
        assert!(
            !response.to_command().no_dataset,
            "the item travels as a dataset"
        );
    }

    #[test]
    fn terminal_response_has_no_dataset() {
        let request = CFindRequest::study_query(2);
        let response = CFindResponse::success(&request, 5);
        assert!(!response.is_pending());
        assert!(response.identifier.is_none());
        assert!(response.to_command().no_dataset);
        assert_eq!(response.completed, 5);
    }

    #[test]
    fn failure_truncates_the_comment() {
        let request = CFindRequest::study_query(2);
        let response = CFindResponse::failure(&request, status::UNABLE_TO_PROCESS, "x".repeat(500));
        assert!(response.error_comment.as_ref().unwrap().len() <= 64);
        assert_eq!(response.state(), StatusState::Failure);
    }

    #[test]
    fn responses_round_trip_through_a_command() {
        let request = CFindRequest::study_query(4);
        let mut item = Dataset::new();
        item.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        let pending = CFindResponse::pending(&request, item.clone());
        let back = CFindResponse::from_command(&pending.to_command(), Some(item.clone())).unwrap();
        assert_eq!(back.status, status::PENDING);
        assert_eq!(back.identifier.unwrap(), item);
        assert_eq!(back.message_id_being_responded_to, 4);

        let success = CFindResponse::success(&request, 3);
        let back = CFindResponse::from_command(&success.to_command(), None).unwrap();
        assert_eq!(back.status, status::SUCCESS);
        assert_eq!(
            back.affected_sop_class_uid,
            sop_class::STUDY_ROOT_QR_FIND,
            "the supplied (0000,0002) round-trips"
        );
        // C-FIND-RSP carries no sub-operation counters (PS3.7 Table 9.3-4).
        assert_eq!(back.completed, 0);
    }

    #[test]
    fn terminal_response_writes_the_supplied_affected_sop_class_uid() {
        let request = CFindRequest::study_query(3);
        let command = CFindResponse::success(&request, 1).to_command();
        let dataset = command.to_dataset();
        assert_eq!(
            dataset
                .get_str(radicom::model::tag::Tag::AFFECTED_SOP_CLASS_UID)
                .unwrap(),
            sop_class::STUDY_ROOT_QR_FIND,
            "PS3.7 Table 9.3-4 requires (0000,0002) in every C-FIND-RSP"
        );
    }

    #[test]
    fn c_find_response_writes_no_suboperation_counters() {
        let request = CFindRequest::study_query(3);
        let dataset = CFindResponse::success(&request, 7)
            .to_command()
            .to_dataset();
        for tag in [
            radicom::model::tag::Tag::NUMBER_OF_REMAINING_SUBOPERATIONS,
            radicom::model::tag::Tag::NUMBER_OF_COMPLETED_SUBOPERATIONS,
            radicom::model::tag::Tag::NUMBER_OF_FAILED_SUBOPERATIONS,
            radicom::model::tag::Tag::NUMBER_OF_WARNING_SUBOPERATIONS,
        ] {
            assert!(
                !dataset.contains(&tag),
                "C-FIND-RSP must not carry sub-operation counter {tag:?}"
            );
        }
    }

    #[test]
    fn wrong_command_field_is_rejected() {
        let command = Command::request(CommandField::CEchoRq, 1);
        assert!(CFindResponse::from_command(&command, None).is_err());
    }

    #[test]
    fn malformed_response_fields_are_rejected() {
        let mut command = Command::request(CommandField::CFindRsp, 1);
        command.message_id_being_responded_to = Some(1);
        command.affected_sop_class_uid = Some("1.2.3".into());

        command.status = Some(status::PENDING);
        assert!(CFindResponse::from_command(&command, None).is_err());
        command.status = Some(status::SUCCESS);
        assert!(CFindResponse::from_command(&command, Some(Dataset::new())).is_err());

        command.message_id_being_responded_to = None;
        assert!(CFindResponse::from_command(&command, None).is_err());
        command.message_id_being_responded_to = Some(1);
        command.affected_sop_class_uid = None;
        assert!(CFindResponse::from_command(&command, None)
            .unwrap()
            .affected_sop_class_uid
            .is_empty());
    }

    /// A provider yielding a fixed set of items then a terminal response.
    struct FixedProvider {
        items: Vec<Dataset>,
    }

    impl ServiceProvider for FixedProvider {
        fn supported_sop_classes(&self) -> Vec<String> {
            vec![sop_class::STUDY_ROOT_QR_FIND.to_string()]
        }

        fn on_c_find<'a>(
            &'a self,
            request: &'a CFindRequest,
        ) -> BoxStream<'a, Result<CFindResponse>> {
            let items = self.items.clone();
            let total = items.len() as u16;
            Box::pin(
                futures::stream::iter(items)
                    .map(move |item| Ok(CFindResponse::pending(request, item))),
            )
            .chain(futures::stream::once(async move {
                Ok(CFindResponse::success(request, total))
            }))
            .boxed()
        }
    }

    /// The provider stream contract: items come as Pending, then exactly one
    /// terminal response (§11.4). The engine loop driving it lives in `server`.
    #[tokio::test]
    async fn provider_stream_yields_items_then_one_terminal_response() {
        let provider = FixedProvider {
            items: vec![Dataset::new(), Dataset::new()],
        };
        let request = CFindRequest::study_query(1);

        let mut stream = provider.on_c_find(&request);
        let mut statuses = Vec::new();
        while let Some(response) = stream.next().await {
            statuses.push(response.unwrap().status);
        }
        assert_eq!(
            statuses,
            vec![status::PENDING, status::PENDING, status::SUCCESS]
        );
    }

    #[test]
    fn the_patient_and_image_queries_use_their_own_key_sets() {
        let patient = CFindRequest::patient_query(1);
        assert_eq!(
            patient
                .identifier
                .get_str(Tag::QUERY_RETRIEVE_LEVEL)
                .unwrap(),
            "PATIENT"
        );
        let keys = patient.identifier.len();
        assert!(
            patient.identifier.contains(&Tag::new(0x0010, 0x0010)),
            "the patient level returns the PatientName"
        );

        let image = CFindRequest::image_query(2);
        assert_eq!(
            image.identifier.get_str(Tag::QUERY_RETRIEVE_LEVEL).unwrap(),
            "IMAGE"
        );
        assert!(
            image.identifier.contains(&Tag::new(0x0008, 0x0018)),
            "the image level returns the SOP instance UID"
        );
        assert_ne!(keys, image.identifier.len(), "each level has its own keys");
    }
}
