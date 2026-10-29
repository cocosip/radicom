//! C-FIND (SRV-04; docs/01-architecture.md §11).
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

    /// The SOP class this level queries under (§7.3).
    pub fn sop_class(self, root: QueryRoot) -> &'static str {
        use crate::association::sop_class as sc;
        match (root, self) {
            (_, QueryRetrieveLevel::Worklist) => sc::MODALITY_WORKLIST_FIND,
            (QueryRoot::Patient, _) => sc::PATIENT_ROOT_QR_FIND,
            (QueryRoot::Study, _) => sc::STUDY_ROOT_QR_FIND,
        }
    }

    /// The retrieve SOP class for this level (§7.4, §7.5).
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

    /// The C-MOVE SOP class for this level (§7.5).
    pub fn move_sop_class(self, root: QueryRoot) -> Option<&'static str> {
        self.retrieve_sop_class(root, RetrieveService::Move)
    }

    /// The C-GET SOP class for this level (§7.4).
    pub fn get_sop_class(self, root: QueryRoot) -> Option<&'static str> {
        self.retrieve_sop_class(root, RetrieveService::Get)
    }
}

/// Which Q/R information model the query uses (§7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryRoot {
    Patient,
    #[default]
    Study,
}

/// The retrieve service a request belongs to (§7.4, §7.5).
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

/// A C-FIND request (§7.3).
#[derive(Debug, Clone)]
pub struct CFindRequest {
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
        identifier.insert_str(Tag::QUERY_RETRIEVE_LEVEL, Vr::Cs, level.as_str());
        for (group, element, vr) in level_keys(level) {
            identifier.insert_empty(Tag::new(*group, *element), *vr);
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
    pub fn with_key(mut self, group: u16, element: u16, vr: Vr, value: impl Into<String>) -> Self {
        self.identifier
            .insert_str(Tag::new(group, element), vr, value);
        self
    }

    /// The query level, when the Identifier states one.
    pub fn level(&self) -> Option<QueryRetrieveLevel> {
        self.identifier
            .try_get_str(Tag::QUERY_RETRIEVE_LEVEL)
            .and_then(|s| QueryRetrieveLevel::parse(&s))
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
        // worklist items are shaped like study-level results
        QueryRetrieveLevel::Worklist => STUDY_KEYS,
    }
}

/// A C-FIND response (§7.3).
#[derive(Debug, Clone)]
pub struct CFindResponse {
    pub message_id_being_responded_to: u16,
    pub status: u16,
    /// Present on Pending responses: one matching item.
    pub identifier: Option<Dataset>,
    /// Sub-operation counters, meaningful on the terminal response.
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
        command.status = Some(self.status);
        command.no_dataset = self.identifier.is_none();
        command.error_comment = self.error_comment.clone();
        if self.completed > 0 || self.failed > 0 || self.warning > 0 || self.remaining > 0 {
            command.completed_suboperations = Some(self.completed);
            command.failed_suboperations = Some(self.failed);
            command.warning_suboperations = Some(self.warning);
            command.remaining_suboperations = Some(self.remaining);
        }
        command
    }

    pub fn from_command(command: &Command, identifier: Option<Dataset>) -> Result<Self> {
        if command.field != CommandField::CFindRsp {
            return Err(NetworkError::protocol("not a C-FIND response"));
        }
        Ok(CFindResponse {
            message_id_being_responded_to: command.message_id_being_responded_to.unwrap_or(0),
            status: command.status.unwrap_or(status::PROCESSING_FAILURE),
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
        assert_eq!(back.completed, 3);
    }

    #[test]
    fn wrong_command_field_is_rejected() {
        let command = Command::request(CommandField::CEchoRq, 1);
        assert!(CFindResponse::from_command(&command, None).is_err());
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
    /// terminal response (§7.3). The engine loop driving it lives in `server`.
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
}
