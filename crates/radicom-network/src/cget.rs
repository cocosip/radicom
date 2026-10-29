//! C-GET (SRV-05; docs/01-architecture.md §11).
//!
//! A C-GET asks the SCP to send instances back on the *same* association. The
//! SCP issues C-STORE sub-operations there, so the SCU must answer them while
//! its C-GET is still in flight — that is what the reverse-store handler is for.
//!
//! The engine owns the single reader, so the provider hands instances to it as
//! [`CGetEvent::Instance`] items instead of writing to the socket itself.

use crate::cfind::{QueryRetrieveLevel, QueryRoot};
use crate::cmove::SubOperationSummary;
use crate::dimse::{status, Command, CommandField, Priority, StatusState};
use crate::error::{NetworkError, Result};
use radicom::io::file::DicomFile;
use radicom::model::dataset::Dataset;
use radicom::model::tag::Tag;
use radicom::model::vr::Vr;

/// A C-GET request (§7.4).
#[derive(Debug, Clone)]
pub struct CGetRequest {
    pub message_id: u16,
    /// The retrieve model's abstract syntax, derived from level + root.
    pub sop_class_uid: String,
    pub priority: Priority,
    /// The Identifier: matching keys, level included.
    pub identifier: Dataset,
}

impl CGetRequest {
    /// A retrieve request at `level`.
    ///
    /// A worklist has no retrieve model, so that combination is an error.
    pub fn new(level: QueryRetrieveLevel, root: QueryRoot, message_id: u16) -> Result<Self> {
        let sop_class_uid = level.get_sop_class(root).ok_or(NetworkError::protocol(
            "C-GET has no worklist information model",
        ))?;
        let mut identifier = Dataset::new();
        identifier.insert_str(Tag::QUERY_RETRIEVE_LEVEL, Vr::Cs, level.as_str());
        Ok(CGetRequest {
            message_id,
            sop_class_uid: sop_class_uid.to_string(),
            priority: Priority::Medium,
            identifier,
        })
    }

    /// A study-level retrieve under the Study Root model.
    pub fn study(study_instance_uid: impl Into<String>, message_id: u16) -> Self {
        Self::level_with_keys(
            QueryRetrieveLevel::Study,
            message_id,
            &[(0x0020, 0x000D, Vr::Ui, study_instance_uid.into())],
        )
    }

    /// A series-level retrieve under the Study Root model.
    pub fn series(
        study_instance_uid: impl Into<String>,
        series_instance_uid: impl Into<String>,
        message_id: u16,
    ) -> Self {
        Self::level_with_keys(
            QueryRetrieveLevel::Series,
            message_id,
            &[
                (0x0020, 0x000D, Vr::Ui, study_instance_uid.into()),
                (0x0020, 0x000E, Vr::Ui, series_instance_uid.into()),
            ],
        )
    }

    /// An image-level retrieve under the Study Root model.
    pub fn image(
        study_instance_uid: impl Into<String>,
        series_instance_uid: impl Into<String>,
        sop_instance_uid: impl Into<String>,
        message_id: u16,
    ) -> Self {
        Self::level_with_keys(
            QueryRetrieveLevel::Image,
            message_id,
            &[
                (0x0020, 0x000D, Vr::Ui, study_instance_uid.into()),
                (0x0020, 0x000E, Vr::Ui, series_instance_uid.into()),
                (0x0008, 0x0018, Vr::Ui, sop_instance_uid.into()),
            ],
        )
    }

    fn level_with_keys(
        level: QueryRetrieveLevel,
        message_id: u16,
        keys: &[(u16, u16, Vr, String)],
    ) -> Self {
        let mut request = CGetRequest::new(level, QueryRoot::Study, message_id)
            .expect("Q/R levels have a retrieve model");
        for (group, element, vr, value) in keys {
            request
                .identifier
                .insert_str(Tag::new(*group, *element), *vr, value.clone());
        }
        request
    }

    /// Adds a matching key.
    pub fn with_key(mut self, group: u16, element: u16, vr: Vr, value: impl Into<String>) -> Self {
        self.identifier
            .insert_str(Tag::new(group, element), vr, value);
        self
    }

    /// The retrieve level, when the Identifier states one.
    pub fn level(&self) -> Option<QueryRetrieveLevel> {
        self.identifier
            .try_get_str(Tag::QUERY_RETRIEVE_LEVEL)
            .and_then(|s| QueryRetrieveLevel::parse(&s))
    }

    /// The command for this request; the Identifier travels as the dataset.
    pub fn to_command(&self) -> Command {
        let mut command = Command::request(CommandField::CGetRq, self.message_id);
        command.affected_sop_class_uid = Some(self.sop_class_uid.clone());
        command.priority = self.priority;
        command.no_dataset = false;
        command
    }
}

/// A C-GET response (§7.4).
///
/// A C-GET response never carries a dataset: the instances arrive as reverse
/// C-STORE sub-operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CGetResponse {
    pub message_id_being_responded_to: u16,
    pub affected_sop_class_uid: String,
    pub status: u16,
    pub remaining: u16,
    pub summary: SubOperationSummary,
    pub error_comment: Option<String>,
}

impl CGetResponse {
    /// A Pending response reporting progress.
    pub fn pending(request: &CGetRequest, remaining: u16, summary: SubOperationSummary) -> Self {
        CGetResponse {
            message_id_being_responded_to: request.message_id,
            affected_sop_class_uid: request.sop_class_uid.clone(),
            status: status::PENDING,
            remaining,
            summary,
            error_comment: None,
        }
    }

    /// The terminal response, its status derived from the counters.
    pub fn completed(request: &CGetRequest, summary: SubOperationSummary) -> Self {
        CGetResponse {
            message_id_being_responded_to: request.message_id,
            affected_sop_class_uid: request.sop_class_uid.clone(),
            status: summary.final_status(),
            remaining: 0,
            summary,
            error_comment: None,
        }
    }

    /// A terminal failure response.
    pub fn failure(request: &CGetRequest, code: u16, comment: impl Into<String>) -> Self {
        CGetResponse {
            message_id_being_responded_to: request.message_id,
            affected_sop_class_uid: request.sop_class_uid.clone(),
            status: code,
            remaining: 0,
            summary: SubOperationSummary::default(),
            error_comment: Some(crate::dimse::truncate_lo(&comment.into())),
        }
    }

    pub fn is_pending(&self) -> bool {
        self.state() == StatusState::Pending
    }

    pub fn state(&self) -> StatusState {
        crate::dimse::classify(self.status)
    }

    pub fn to_command(&self) -> Command {
        let mut command =
            Command::request(CommandField::CGetRsp, self.message_id_being_responded_to);
        command.message_id_being_responded_to = Some(self.message_id_being_responded_to);
        command.affected_sop_class_uid = Some(self.affected_sop_class_uid.clone());
        command.status = Some(self.status);
        command.no_dataset = true;
        command.error_comment = self.error_comment.clone();
        command.remaining_suboperations = Some(self.remaining);
        command.completed_suboperations = Some(self.summary.completed);
        command.failed_suboperations = Some(self.summary.failed);
        command.warning_suboperations = Some(self.summary.warning);
        command
    }

    pub fn from_command(command: &Command) -> Result<Self> {
        if command.field != CommandField::CGetRsp {
            return Err(NetworkError::protocol("not a C-GET response"));
        }
        Ok(CGetResponse {
            message_id_being_responded_to: command.message_id_being_responded_to.unwrap_or(0),
            affected_sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
            status: command.status.unwrap_or(status::PROCESSING_FAILURE),
            remaining: command.remaining_suboperations.unwrap_or(0),
            summary: SubOperationSummary {
                completed: command.completed_suboperations.unwrap_or(0),
                failed: command.failed_suboperations.unwrap_or(0),
                warning: command.warning_suboperations.unwrap_or(0),
            },
            error_comment: command.error_comment.clone(),
        })
    }
}

/// What a C-GET provider yields (§7.4).
///
/// The engine owns the socket, so the provider cannot send a C-STORE itself:
/// it hands the instance over and the engine performs the sub-operation. A
/// provider can interleave Pending [`CGetResponse`] values to report the real
/// remaining-sub-operation count while instances are delivered.
#[derive(Debug, Clone)]
pub enum CGetEvent {
    /// Deliver this instance to the SCU as a C-STORE sub-operation.
    Instance(Box<DicomFile>),
    /// Relay this response to the SCU.
    Response(CGetResponse),
}

impl CGetEvent {
    /// Convenience for the common case.
    pub fn instance(file: DicomFile) -> Self {
        CGetEvent::Instance(Box::new(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::association::sop_class;

    #[test]
    fn levels_map_to_the_get_sop_classes() {
        assert_eq!(
            QueryRetrieveLevel::Study.get_sop_class(QueryRoot::Study),
            Some(sop_class::STUDY_ROOT_QR_GET)
        );
        assert_eq!(
            QueryRetrieveLevel::Patient.get_sop_class(QueryRoot::Patient),
            Some(sop_class::PATIENT_ROOT_QR_GET)
        );
        assert_eq!(
            QueryRetrieveLevel::Worklist.get_sop_class(QueryRoot::Study),
            None
        );
        // the get and move models must not be confused
        assert_ne!(
            QueryRetrieveLevel::Study.get_sop_class(QueryRoot::Study),
            QueryRetrieveLevel::Study.move_sop_class(QueryRoot::Study)
        );
    }

    #[test]
    fn worklist_level_cannot_be_retrieved() {
        assert!(CGetRequest::new(QueryRetrieveLevel::Worklist, QueryRoot::Study, 1).is_err());
    }

    #[test]
    fn study_retrieve_carries_the_study_uid() {
        let request = CGetRequest::study("1.2.3", 5);
        assert_eq!(request.level(), Some(QueryRetrieveLevel::Study));
        assert_eq!(request.sop_class_uid, sop_class::STUDY_ROOT_QR_GET);
        assert_eq!(
            request.identifier.get_str(Tag::STUDY_INSTANCE_UID).unwrap(),
            "1.2.3"
        );

        let command = request.to_command();
        assert_eq!(command.field, CommandField::CGetRq);
        assert!(!command.no_dataset, "the Identifier travels as a dataset");
    }

    #[test]
    fn series_and_image_retrieves_add_their_keys() {
        let series = CGetRequest::series("1.1", "1.2", 1);
        assert_eq!(series.level(), Some(QueryRetrieveLevel::Series));
        let image = CGetRequest::image("1.1", "1.2", "1.3", 2);
        assert_eq!(image.level(), Some(QueryRetrieveLevel::Image));
        assert_eq!(
            image.identifier.get_str(Tag::SOP_INSTANCE_UID).unwrap(),
            "1.3"
        );
    }

    #[test]
    fn terminal_status_follows_the_counters() {
        let request = CGetRequest::study("1.2.3", 1);
        assert_eq!(
            CGetResponse::completed(
                &request,
                SubOperationSummary {
                    completed: 2,
                    failed: 0,
                    warning: 0
                }
            )
            .status,
            status::SUCCESS
        );
        assert_eq!(
            CGetResponse::completed(
                &request,
                SubOperationSummary {
                    completed: 0,
                    failed: 1,
                    warning: 0
                }
            )
            .status,
            status::UNABLE_TO_PERFORM_SUBOPERATIONS
        );
    }

    #[test]
    fn responses_round_trip_through_a_command() {
        let request = CGetRequest::study("1.2.3", 8);
        let response = CGetResponse::pending(
            &request,
            3,
            SubOperationSummary {
                completed: 1,
                failed: 0,
                warning: 0,
            },
        );
        let back = CGetResponse::from_command(&response.to_command()).unwrap();
        assert_eq!(back, response);
        assert!(back.is_pending());
        assert_eq!(back.remaining, 3);
    }

    #[test]
    fn failure_truncates_the_comment() {
        let request = CGetRequest::study("1.2.3", 1);
        let response = CGetResponse::failure(&request, status::UNABLE_TO_PROCESS, "x".repeat(500));
        assert_eq!(response.error_comment.as_ref().unwrap().len(), 64);
    }

    #[test]
    fn wrong_command_field_is_rejected() {
        assert!(CGetResponse::from_command(&Command::request(CommandField::CFindRsp, 1)).is_err());
    }
}
