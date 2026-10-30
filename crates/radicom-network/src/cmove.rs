//! C-MOVE (SRV-06; docs/01-architecture.md §11).
//!
//! A C-MOVE asks the SCP to push instances to a *different* peer. The SCP
//! answers with Pending progress and one terminal response while it opens its
//! own association toward the destination and issues C-STORE sub-operations
//! there. The outbound side is [`store_to_destination`], so a provider never
//! has to speak DIMSE itself.

use crate::cfind::{QueryRetrieveLevel, QueryRoot};
use crate::dimse::{status, Command, CommandField, Priority, StatusState};
use crate::error::{NetworkError, Result};
use crate::server::{Association, ClientOptions};
use radicom::io::file::DicomFile;
use radicom::model::dataset::Dataset;
use radicom::model::tag::Tag;
use radicom::model::vr::Vr;

/// Sub-operation counters shared by the retrieve services (§11.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubOperationSummary {
    pub completed: u16,
    pub failed: u16,
    pub warning: u16,
}

impl SubOperationSummary {
    /// Total instances the operation covered; a warning counts once.
    pub fn total(&self) -> u16 {
        self.completed
            .saturating_add(self.failed)
            .saturating_add(self.warning)
    }

    /// Folds one sub-operation's terminal status into the counters.
    ///
    /// Success, Warning and Failure are distinct outcome buckets (PS3.4 C.4).
    /// Cancel and Pending are not instance
    /// outcomes at all and must not pollute the counts.
    pub fn record(&mut self, state: StatusState) {
        match state {
            StatusState::Success => self.completed = self.completed.saturating_add(1),
            StatusState::Warning => {
                self.warning = self.warning.saturating_add(1);
            }
            StatusState::Failure => self.failed = self.failed.saturating_add(1),
            StatusState::Cancel | StatusState::Pending => {}
        }
    }

    /// The status the terminal response carries (PS3.4 C-MOVE/C-GET):
    /// nothing failed → Success; some instances came back → warnings; none did
    /// → "unable to perform sub-operations".
    pub fn final_status(&self) -> u16 {
        if self.failed == 0 && self.warning == 0 {
            return status::SUCCESS;
        }
        if self.completed == 0 && self.warning == 0 {
            return status::UNABLE_TO_PERFORM_SUBOPERATIONS;
        }
        status::SUBOPERATIONS_COMPLETED_WITH_WARNINGS
    }
}

/// A C-MOVE request (§11.4).
#[derive(Debug, Clone)]
pub struct CMoveRequest {
    pub message_id: u16,
    /// The retrieve model's abstract syntax, derived from level + root.
    pub sop_class_uid: String,
    pub priority: Priority,
    /// The AE title the SCP must deliver to; required by the standard.
    pub destination_ae: String,
    /// The Identifier: matching keys, level included.
    pub identifier: Dataset,
}

impl CMoveRequest {
    /// A retrieve request at `level`.
    ///
    /// The Identifier starts with only the level element; matching keys are
    /// added with [`Self::with_key`]. A worklist level has no retrieve model.
    pub fn new(
        level: QueryRetrieveLevel,
        root: QueryRoot,
        destination_ae: impl Into<String>,
        message_id: u16,
    ) -> Result<Self> {
        let sop_class_uid = level.move_sop_class(root).ok_or(NetworkError::protocol(
            "C-MOVE has no worklist information model",
        ))?;
        let mut identifier = Dataset::new();
        identifier.insert_str(Tag::QUERY_RETRIEVE_LEVEL, Vr::Cs, level.as_str());
        Ok(CMoveRequest {
            message_id,
            sop_class_uid: sop_class_uid.to_string(),
            priority: Priority::Medium,
            destination_ae: destination_ae.into(),
            identifier,
        })
    }

    /// A study-level retrieve under the Study Root model.
    pub fn study(
        study_instance_uid: impl Into<String>,
        destination_ae: impl Into<String>,
        message_id: u16,
    ) -> Self {
        Self::level_with_keys(
            QueryRetrieveLevel::Study,
            destination_ae,
            message_id,
            &[(0x0020, 0x000D, Vr::Ui, study_instance_uid.into())],
        )
    }

    /// A series-level retrieve under the Study Root model.
    pub fn series(
        study_instance_uid: impl Into<String>,
        series_instance_uid: impl Into<String>,
        destination_ae: impl Into<String>,
        message_id: u16,
    ) -> Self {
        Self::level_with_keys(
            QueryRetrieveLevel::Series,
            destination_ae,
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
        destination_ae: impl Into<String>,
        message_id: u16,
    ) -> Self {
        Self::level_with_keys(
            QueryRetrieveLevel::Image,
            destination_ae,
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
        destination_ae: impl Into<String>,
        message_id: u16,
        keys: &[(u16, u16, Vr, String)],
    ) -> Self {
        // the level/root pair always has a retrieve model, so `new` cannot fail
        let mut request = CMoveRequest::new(level, QueryRoot::Study, destination_ae, message_id)
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
        let mut command = Command::request(CommandField::CMoveRq, self.message_id);
        command.affected_sop_class_uid = Some(self.sop_class_uid.clone());
        command.priority = self.priority;
        command.move_destination = Some(self.destination_ae.clone());
        command.no_dataset = false;
        command
    }
}

/// A C-MOVE response (§11.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CMoveResponse {
    pub message_id_being_responded_to: u16,
    /// Echoed on every response, as PS3.7 requires.
    pub affected_sop_class_uid: String,
    pub status: u16,
    /// Instances still to be delivered.
    pub remaining: u16,
    pub summary: SubOperationSummary,
    pub error_comment: Option<String>,
    /// SOP instances whose C-STORE sub-operations failed.
    pub failed_sop_instance_uids: Vec<String>,
}

impl CMoveResponse {
    /// A Pending response reporting progress.
    pub fn pending(request: &CMoveRequest, remaining: u16, summary: SubOperationSummary) -> Self {
        CMoveResponse {
            message_id_being_responded_to: request.message_id,
            affected_sop_class_uid: request.sop_class_uid.clone(),
            status: status::PENDING,
            remaining,
            summary,
            error_comment: None,
            failed_sop_instance_uids: Vec::new(),
        }
    }

    /// The terminal response, its status derived from the counters.
    pub fn completed(request: &CMoveRequest, summary: SubOperationSummary) -> Self {
        CMoveResponse {
            message_id_being_responded_to: request.message_id,
            affected_sop_class_uid: request.sop_class_uid.clone(),
            status: summary.final_status(),
            remaining: 0,
            summary,
            error_comment: None,
            failed_sop_instance_uids: Vec::new(),
        }
    }

    /// A terminal failure response.
    pub fn failure(request: &CMoveRequest, code: u16, comment: impl Into<String>) -> Self {
        CMoveResponse {
            message_id_being_responded_to: request.message_id,
            affected_sop_class_uid: request.sop_class_uid.clone(),
            status: code,
            remaining: 0,
            summary: SubOperationSummary::default(),
            error_comment: Some(crate::dimse::truncate_lo(&comment.into())),
            failed_sop_instance_uids: Vec::new(),
        }
    }

    /// True when more responses follow this one.
    pub fn is_pending(&self) -> bool {
        self.state() == StatusState::Pending
    }

    pub fn state(&self) -> StatusState {
        crate::dimse::classify(self.status)
    }

    /// The command for this response; a failed-instance Identifier may follow.
    pub fn to_command(&self) -> Command {
        let mut command =
            Command::request(CommandField::CMoveRsp, self.message_id_being_responded_to);
        command.message_id_being_responded_to = Some(self.message_id_being_responded_to);
        command.affected_sop_class_uid = Some(self.affected_sop_class_uid.clone());
        command.status = Some(self.status);
        command.no_dataset = self.identifier().is_none();
        command.error_comment = self.error_comment.clone();
        command.remaining_suboperations =
            matches!(self.state(), StatusState::Pending | StatusState::Cancel)
                .then_some(self.remaining);
        command.completed_suboperations = Some(self.summary.completed);
        command.failed_suboperations = Some(self.summary.failed);
        command.warning_suboperations = Some(self.summary.warning);
        command
    }

    /// The failed-instance Identifier of a terminal Warning/Failure/Cancel response.
    pub fn identifier(&self) -> Option<Dataset> {
        if !matches!(
            self.state(),
            StatusState::Warning | StatusState::Failure | StatusState::Cancel
        ) || self.failed_sop_instance_uids.is_empty()
        {
            return None;
        }
        let mut dataset = Dataset::new();
        dataset.insert_str(
            Tag::new(0x0008, 0x0058),
            Vr::Ui,
            self.failed_sop_instance_uids.join("\\"),
        );
        Some(dataset)
    }

    /// Attaches the failed-instance Identifier read after the command.
    pub fn set_identifier(&mut self, identifier: &Dataset) {
        self.failed_sop_instance_uids = identifier
            .try_get_str(Tag::new(0x0008, 0x0058))
            .map(|value| {
                value
                    .split('\\')
                    .filter(|uid| !uid.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
    }

    pub fn from_command(command: &Command) -> Result<Self> {
        if command.field != CommandField::CMoveRsp {
            return Err(NetworkError::protocol("not a C-MOVE response"));
        }
        Ok(CMoveResponse {
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
            failed_sop_instance_uids: Vec::new(),
        })
    }
}

/// Sends instances to `destination` as C-MOVE sub-operations (§11.4).
///
/// The SCP owns the AE→address mapping; this helper takes the resolved address.
/// Every instance is pushed with the Move Originator AE title and message ID set
/// (divergence D5: always written), so the destination can report back.
///
/// Returns the counters plus the transport error that ended the batch early, if
/// any: a failed destination must not erase the sub-operations already counted
/// (NET-16). Only a failure *before* the first instance (connect/negotiate) is
/// returned as `Err`.
pub async fn store_to_destination(
    destination: impl tokio::net::ToSocketAddrs,
    calling_ae: &str,
    called_ae: &str,
    files: &[DicomFile],
    originator: Option<(&str, u16)>,
    options: ClientOptions,
) -> Result<(SubOperationSummary, Option<NetworkError>)> {
    let (summary, _, error) = store_to_destination_with_failures(
        destination,
        calling_ae,
        called_ae,
        files,
        originator,
        options,
    )
    .await?;
    Ok((summary, error))
}

/// Delivers a MOVE batch and returns the Failed SOP Instance UID List alongside
/// its counters and any transport error. Providers attach this list to their
/// terminal [`CMoveResponse`].
pub async fn store_to_destination_with_failures(
    destination: impl tokio::net::ToSocketAddrs,
    calling_ae: &str,
    called_ae: &str,
    files: &[DicomFile],
    originator: Option<(&str, u16)>,
    options: ClientOptions,
) -> Result<(SubOperationSummary, Vec<String>, Option<NetworkError>)> {
    let mut summary = SubOperationSummary::default();
    let mut failed_uids = Vec::new();
    if files.is_empty() {
        return Ok((summary, failed_uids, None));
    }

    // one presentation context per (storage class, syntax) the batch needs
    let mut contexts = crate::association::ContextSetBuilder::new();
    for file in files {
        let sop_class = file
            .dataset
            .try_get_str(Tag::SOP_CLASS_UID)
            .map(|s| s.trim().to_string())
            .ok_or(NetworkError::protocol("instance has no SOP class UID"))?;
        let syntax = file.dataset.transfer_syntax().uid().to_string();
        contexts.add(
            sop_class,
            crate::services::c_store_transfer_syntaxes(&syntax, &[], false),
        )?;
    }

    let mut association = Association::open(
        destination,
        calling_ae,
        called_ae,
        contexts.to_wire(),
        options,
    )
    .await?;
    let (originator_ae, originator_id) = match originator {
        Some((ae, id)) => (Some(ae), Some(id)),
        None => (None, None),
    };
    for (index, file) in files.iter().enumerate() {
        match association
            .send_c_store_with_originator(file, originator_ae, originator_id)
            .await
        {
            Ok(response) => {
                summary.record(response.state());
                if response.state() == StatusState::Failure {
                    failed_uids.push(response.affected_sop_instance_uid);
                }
            }
            Err(error) => {
                // A broken destination ends the batch, but the instances already
                // delivered must stay countable: PS3.4 §C.4.2 reports progress
                // through the sub-operation counters, not by discarding them
                // (NET-16). The error travels back alongside them so the caller
                // can report it in the terminal response.
                for undelivered in &files[index..] {
                    summary.failed = summary.failed.saturating_add(1);
                    if let Some(uid) = undelivered.dataset.try_get_str(Tag::SOP_INSTANCE_UID) {
                        failed_uids.push(uid.trim().to_string());
                    }
                }
                let _ = association.release().await;
                return Ok((summary, failed_uids, Some(error)));
            }
        }
    }
    // the deliveries above are the result; a peer that fumbles the release
    // afterwards must not turn them into a failure
    let _ = association.release().await;
    Ok((summary, failed_uids, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::association::sop_class;

    fn sample_file(instance_uid: &str) -> DicomFile {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, instance_uid);
        dataset.set_transfer_syntax(
            radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
        );
        DicomFile::from_dataset(dataset)
    }

    #[test]
    fn levels_map_to_the_retrieve_sop_classes() {
        assert_eq!(
            QueryRetrieveLevel::Study.move_sop_class(QueryRoot::Study),
            Some(sop_class::STUDY_ROOT_QR_MOVE)
        );
        assert_eq!(
            QueryRetrieveLevel::Image.move_sop_class(QueryRoot::Patient),
            Some(sop_class::PATIENT_ROOT_QR_MOVE)
        );
        assert_eq!(
            QueryRetrieveLevel::Worklist.move_sop_class(QueryRoot::Study),
            None,
            "a worklist has no retrieve model"
        );
    }

    #[test]
    fn worklist_level_cannot_be_retrieved() {
        let error = CMoveRequest::new(QueryRetrieveLevel::Worklist, QueryRoot::Study, "DEST", 1);
        assert!(error.is_err());
    }

    #[test]
    fn study_retrieve_carries_the_study_uid_and_destination() {
        let request = CMoveRequest::study("1.2.3", "ARCHIVE", 7);
        assert_eq!(request.level(), Some(QueryRetrieveLevel::Study));
        assert_eq!(request.sop_class_uid, sop_class::STUDY_ROOT_QR_MOVE);
        assert_eq!(
            request.identifier.get_str(Tag::STUDY_INSTANCE_UID).unwrap(),
            "1.2.3"
        );

        let command = request.to_command();
        assert_eq!(command.field, CommandField::CMoveRq);
        assert_eq!(command.move_destination.as_deref(), Some("ARCHIVE"));
        assert!(!command.no_dataset, "the Identifier travels as a dataset");
    }

    #[test]
    fn series_and_image_retrieves_add_their_keys() {
        let series = CMoveRequest::series("1.1", "1.2", "DEST", 1);
        assert_eq!(series.level(), Some(QueryRetrieveLevel::Series));
        assert_eq!(
            series.identifier.get_str(Tag::SERIES_INSTANCE_UID).unwrap(),
            "1.2"
        );

        let image = CMoveRequest::image("1.1", "1.2", "1.3", "DEST", 2);
        assert_eq!(image.level(), Some(QueryRetrieveLevel::Image));
        assert_eq!(
            image.identifier.get_str(Tag::SOP_INSTANCE_UID).unwrap(),
            "1.3"
        );
        assert_eq!(image.message_id, 2);
    }

    #[test]
    fn pending_response_reports_progress() {
        let request = CMoveRequest::study("1.2.3", "ARCHIVE", 4);
        let summary = SubOperationSummary {
            completed: 1,
            failed: 0,
            warning: 0,
        };
        let response = CMoveResponse::pending(&request, 2, summary);
        assert!(response.is_pending());
        assert_eq!(response.remaining, 2);
        assert_eq!(
            response.affected_sop_class_uid,
            sop_class::STUDY_ROOT_QR_MOVE
        );
        let command = response.to_command();
        assert!(command.no_dataset);
        assert_eq!(command.remaining_suboperations, Some(2));
        assert_eq!(command.completed_suboperations, Some(1));
    }

    #[test]
    fn terminal_status_follows_the_counters() {
        let request = CMoveRequest::study("1.2.3", "ARCHIVE", 4);
        let ok = CMoveResponse::completed(
            &request,
            SubOperationSummary {
                completed: 3,
                failed: 0,
                warning: 0,
            },
        );
        assert_eq!(ok.status, status::SUCCESS);

        let warned = CMoveResponse::completed(
            &request,
            SubOperationSummary {
                completed: 2,
                failed: 0,
                warning: 1,
            },
        );
        assert_eq!(warned.status, status::SUBOPERATIONS_COMPLETED_WITH_WARNINGS);
        assert_eq!(warned.state(), StatusState::Warning);

        let partial = CMoveResponse::completed(
            &request,
            SubOperationSummary {
                completed: 1,
                failed: 1,
                warning: 0,
            },
        );
        assert_eq!(
            partial.status,
            status::SUBOPERATIONS_COMPLETED_WITH_WARNINGS
        );

        let nothing = CMoveResponse::completed(
            &request,
            SubOperationSummary {
                completed: 0,
                failed: 2,
                warning: 0,
            },
        );
        assert_eq!(nothing.status, status::UNABLE_TO_PERFORM_SUBOPERATIONS);
        assert_eq!(nothing.state(), StatusState::Failure);
    }

    #[test]
    fn responses_round_trip_through_a_command() {
        let request = CMoveRequest::study("1.2.3", "ARCHIVE", 9);
        let response = CMoveResponse::completed(
            &request,
            SubOperationSummary {
                completed: 2,
                failed: 1,
                warning: 0,
            },
        );
        let back = CMoveResponse::from_command(&response.to_command()).unwrap();
        assert_eq!(back, response);
    }

    #[test]
    fn failure_truncates_the_comment() {
        let request = CMoveRequest::study("1.2.3", "ARCHIVE", 1);
        let response =
            CMoveResponse::failure(&request, status::MOVE_DESTINATION_UNKNOWN, "x".repeat(500));
        assert_eq!(response.error_comment.as_ref().unwrap().len(), 64);
        assert_eq!(response.state(), StatusState::Failure);
    }

    #[test]
    fn warning_only_retrieve_has_no_failed_instance_identifier() {
        let response = CMoveResponse::completed(
            &CMoveRequest::study("1.2.3", "DEST", 1),
            SubOperationSummary {
                completed: 0,
                failed: 0,
                warning: 2,
            },
        );
        assert_eq!(
            response.status,
            status::SUBOPERATIONS_COMPLETED_WITH_WARNINGS
        );
        // PS3.4 C.4: no failed sub-operation means no Failed UID List or dataset.
        assert!(response.identifier().is_none());
        assert!(response.to_command().no_dataset);
    }

    #[test]
    fn terminal_retrieve_omits_remaining_but_cancel_keeps_it() {
        let request = CMoveRequest::study("1.2.3", "ARCHIVE", 1);
        let mut response = CMoveResponse::completed(&request, SubOperationSummary::default());
        assert_eq!(response.to_command().remaining_suboperations, None);
        response.status = status::CANCEL;
        response.remaining = 3;
        assert_eq!(response.to_command().remaining_suboperations, Some(3));
    }

    #[test]
    fn wrong_command_field_is_rejected() {
        let command = Command::request(CommandField::CFindRsp, 1);
        assert!(CMoveResponse::from_command(&command).is_err());
    }

    #[test]
    fn summary_counts_each_outcome() {
        let mut summary = SubOperationSummary::default();
        summary.record(StatusState::Success);
        summary.record(StatusState::Warning);
        summary.record(StatusState::Failure);
        assert_eq!(
            summary.completed, 1,
            "only successful sub-operations complete"
        );
        assert_eq!(summary.warning, 1, "the warned instance is flagged as well");
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.total(), 3, "three instances, counted once each");
    }

    #[test]
    fn cancels_and_pending_are_not_instance_outcomes() {
        // PS3.7 status categories: Cancel acknowledges a request and Pending
        // reports progress; neither is the outcome of a sub-operation, so
        // neither may be counted as a failure or a completion.
        let mut summary = SubOperationSummary::default();
        summary.record(StatusState::Cancel);
        summary.record(StatusState::Pending);
        assert_eq!(summary, SubOperationSummary::default());
        assert_eq!(summary.total(), 0);
        assert_eq!(summary.final_status(), status::SUCCESS);

        summary.record(StatusState::Success);
        summary.record(StatusState::Cancel);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 0);
    }

    #[test]
    fn an_all_warned_retrieve_is_not_a_failure() {
        // every instance came back, each with a warning
        let mut summary = SubOperationSummary::default();
        for _ in 0..3 {
            summary.record(StatusState::Warning);
        }
        assert_eq!(summary.completed, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(
            summary.final_status(),
            status::SUBOPERATIONS_COMPLETED_WITH_WARNINGS
        );
    }

    #[tokio::test]
    async fn an_empty_batch_needs_no_association() {
        // an unreachable address proves no connection is attempted
        let (summary, error) = store_to_destination(
            "127.0.0.1:1",
            "SCP",
            "ARCHIVE",
            &[],
            None,
            ClientOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(summary.total(), 0);
        assert!(error.is_none(), "an empty batch attempts nothing");
    }

    #[test]
    fn sample_file_helper_is_usable() {
        let file = sample_file("1.2.3.4");
        assert_eq!(
            file.dataset.get_str(Tag::SOP_INSTANCE_UID).unwrap(),
            "1.2.3.4"
        );
    }

    #[test]
    fn a_query_key_can_be_added_fluently() {
        let request = CMoveRequest::study("1.2.3", "DEST", 1).with_key(
            0x0008,
            0x0052,
            radicom::model::vr::Vr::Cs,
            "T-D0100",
        );
        assert_eq!(
            request
                .identifier
                .get_str(Tag::new(0x0008, 0x0052))
                .unwrap(),
            "T-D0100"
        );
    }
}
