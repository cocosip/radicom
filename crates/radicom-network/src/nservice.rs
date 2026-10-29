//! N-service message types (SRV-07; docs/01-architecture.md §11).
//!
//! The six DIMSE-N services — N-EVENT-REPORT, N-GET, N-SET, N-ACTION, N-CREATE,
//! N-DELETE — share one command shape: affected/requested SOP class and instance
//! UIDs, an optional event type / action type, a status, and an optional dataset.
//! [`NServiceRequest`] and [`NServiceResponse`] wrap that shape over [`Command`],
//! covering all six without a struct per service.

use radicom::model::dataset::Dataset;

use crate::dimse::{Command, CommandField};

/// The six N-service request command fields.
pub const N_REQUEST_FIELDS: [CommandField; 6] = [
    CommandField::NEventReportRq,
    CommandField::NGetRq,
    CommandField::NSetRq,
    CommandField::NActionRq,
    CommandField::NCreateRq,
    CommandField::NDeleteRq,
];

/// True when `field` is one of the six N-service request fields.
pub fn is_n_request(field: CommandField) -> bool {
    N_REQUEST_FIELDS.contains(&field)
}

/// True when this request names its target with `(0000,0003)/(0000,1001)
/// Requested` rather than `(0000,0002)/(0000,1000) Affected` (PS3.7 §10.1).
///
/// N-CREATE and N-EVENT-REPORT act on an *affected* instance; the other four
/// services address a *requested* one.
pub fn uses_requested_uids(field: CommandField) -> bool {
    matches!(
        field,
        CommandField::NGetRq
            | CommandField::NSetRq
            | CommandField::NActionRq
            | CommandField::NDeleteRq
    )
}

/// An N-service request (all six services).
#[derive(Debug, Clone)]
pub struct NServiceRequest {
    pub field: CommandField,
    pub message_id: u16,
    /// The SOP class/instance the request addresses. `to_command` places it in
    /// the Requested or Affected tags that `field` calls for (PS3.7 §10.1).
    pub sop_class_uid: Option<String>,
    pub sop_instance_uid: Option<String>,
    /// Requested attributes for N-GET; an empty list means the element is absent.
    pub attribute_identifier_list: Vec<radicom::model::tag::Tag>,
    pub event_type_id: Option<u16>,
    pub action_type_id: Option<u16>,
    pub dataset: Option<Dataset>,
}

impl NServiceRequest {
    /// A request for `field` with the given message id.
    pub fn new(field: CommandField, message_id: u16) -> Self {
        NServiceRequest {
            field,
            message_id,
            sop_class_uid: None,
            sop_instance_uid: None,
            attribute_identifier_list: Vec::new(),
            event_type_id: None,
            action_type_id: None,
            dataset: None,
        }
    }

    /// Builds the wire command (the dataset, if any, is sent separately).
    pub fn to_command(&self) -> Command {
        let mut command = Command::request(self.field, self.message_id);
        if uses_requested_uids(self.field) {
            command.requested_sop_class_uid = self.sop_class_uid.clone();
            command.requested_sop_instance_uid = self.sop_instance_uid.clone();
        } else {
            command.affected_sop_class_uid = self.sop_class_uid.clone();
            command.affected_sop_instance_uid = self.sop_instance_uid.clone();
        }
        if self.field == CommandField::NGetRq && !self.attribute_identifier_list.is_empty() {
            command.attribute_identifier_list = Some(self.attribute_identifier_list.clone());
        }
        command.event_type_id = self.event_type_id;
        command.action_type_id = self.action_type_id;
        command.no_dataset = self.dataset.is_none();
        command
    }

    /// Parses a request from a received command plus its (optional) dataset.
    pub fn from_command(command: &Command, dataset: Option<Dataset>) -> Self {
        let (sop_class_uid, sop_instance_uid) = if uses_requested_uids(command.field) {
            (
                command.requested_sop_class_uid.clone(),
                command.requested_sop_instance_uid.clone(),
            )
        } else {
            (
                command.affected_sop_class_uid.clone(),
                command.affected_sop_instance_uid.clone(),
            )
        };
        NServiceRequest {
            field: command.field,
            message_id: command.message_id,
            sop_class_uid,
            sop_instance_uid,
            attribute_identifier_list: command
                .attribute_identifier_list
                .clone()
                .unwrap_or_default(),
            event_type_id: command.event_type_id,
            action_type_id: command.action_type_id,
            dataset,
        }
    }
}

/// An N-service response.
#[derive(Debug, Clone)]
pub struct NServiceResponse {
    pub field: CommandField,
    pub message_id_being_responded_to: u16,
    pub affected_sop_class_uid: Option<String>,
    pub affected_sop_instance_uid: Option<String>,
    pub event_type_id: Option<u16>,
    pub action_type_id: Option<u16>,
    pub status: u16,
    pub dataset: Option<Dataset>,
}

impl NServiceResponse {
    /// A success response to `request`, optionally carrying a dataset.
    pub fn success(request: &NServiceRequest, dataset: Option<Dataset>) -> Self {
        let mut response = NServiceResponse::from_request(request, crate::dimse::status::SUCCESS);
        response.dataset = dataset;
        response
    }

    /// A successful N-CREATE response carrying the instance UID assigned by
    /// the SCP. The request may omit an instance UID because the SCP creates it.
    pub fn success_with_new_instance(
        request: &NServiceRequest,
        sop_instance_uid: impl Into<String>,
        dataset: Option<Dataset>,
    ) -> Self {
        let mut response = NServiceResponse::success(request, dataset);
        response.affected_sop_instance_uid = Some(sop_instance_uid.into());
        response
    }

    /// A failure response to `request` with `status`.
    pub fn failure(request: &NServiceRequest, status: u16) -> Self {
        NServiceResponse::from_request(request, status)
    }

    fn from_request(request: &NServiceRequest, status: u16) -> Self {
        NServiceResponse {
            field: crate::dimse::response_field(request.field).unwrap_or(request.field),
            message_id_being_responded_to: request.message_id,
            // PS3.7 §10.1: responses always name the instance as Affected
            affected_sop_class_uid: request.sop_class_uid.clone(),
            affected_sop_instance_uid: request.sop_instance_uid.clone(),
            event_type_id: request.event_type_id,
            action_type_id: request.action_type_id,
            status,
            dataset: None,
        }
    }

    /// Builds the wire command (the dataset, if any, is sent separately).
    pub fn to_command(&self) -> Command {
        let mut command = Command::request(self.field, 0);
        command.message_id_being_responded_to = Some(self.message_id_being_responded_to);
        command.status = Some(self.status);
        command.affected_sop_class_uid = self.affected_sop_class_uid.clone();
        command.affected_sop_instance_uid = self.affected_sop_instance_uid.clone();
        command.event_type_id = self.event_type_id;
        command.action_type_id = self.action_type_id;
        command.no_dataset = self.dataset.is_none();
        command
    }

    /// Parses a response from a received command plus its (optional) dataset.
    pub fn from_command(command: &Command, dataset: Option<Dataset>) -> Self {
        NServiceResponse {
            field: command.field,
            message_id_being_responded_to: command.message_id_being_responded_to.unwrap_or(0),
            affected_sop_class_uid: command.affected_sop_class_uid.clone(),
            affected_sop_instance_uid: command.affected_sop_instance_uid.clone(),
            event_type_id: command.event_type_id,
            action_type_id: command.action_type_id,
            // PS3.7: Status is mandatory in a response. A peer that omits it
            // must not be read as a success: every other service defaults a
            // missing status to ProcessingFailure, and a fail-open answer here
            // would report a malformed response as a completed operation (NET-08).
            status: command
                .status
                .unwrap_or(crate::dimse::status::PROCESSING_FAILURE),
            dataset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dimse::status;
    use crate::test_wire::{command_bytes, element_text, element_value};

    #[test]
    fn every_n_request_field_is_recognised() {
        for field in N_REQUEST_FIELDS {
            assert!(is_n_request(field));
        }
        assert!(!is_n_request(CommandField::CFindRq));
    }

    #[test]
    fn a_request_command_round_trips_through_its_dataset() {
        let mut request = NServiceRequest::new(CommandField::NActionRq, 7);
        request.sop_class_uid = Some("1.2.3".into());
        request.sop_instance_uid = Some("1.2.3.4".into());
        request.action_type_id = Some(1);
        let command = request.to_command();
        let dataset = command.to_dataset();
        let parsed = Command::from_dataset(&dataset).unwrap();
        let back = NServiceRequest::from_command(&parsed, None);
        assert_eq!(back.field, CommandField::NActionRq);
        assert_eq!(back.message_id, 7);
        assert_eq!(back.sop_instance_uid.as_deref(), Some("1.2.3.4"));
        assert_eq!(back.action_type_id, Some(1));
        assert!(back.dataset.is_none());
        assert!(
            parsed.no_dataset,
            "a request without a dataset sets the flag"
        );
    }

    #[test]
    fn requested_services_encode_the_requested_tags() {
        // PS3.7 §10.1 / Table E.1-1: N-GET, N-SET, N-ACTION and N-DELETE name
        // their target in (0000,0003)/(0000,1001) Requested, not Affected.
        for field in [
            CommandField::NGetRq,
            CommandField::NSetRq,
            CommandField::NActionRq,
            CommandField::NDeleteRq,
        ] {
            let mut request = NServiceRequest::new(field, 1);
            request.sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.88.1".into());
            request.sop_instance_uid = Some("1.2.3.4".into());
            let bytes = command_bytes(&request.to_command());
            assert_eq!(
                element_text(&bytes, 0x0000, 0x0003).as_deref(),
                Some("1.2.840.10008.5.1.4.1.1.88.1"),
                "{field:?} must carry (0000,0003) Requested SOP Class UID"
            );
            assert_eq!(
                element_text(&bytes, 0x0000, 0x1001).as_deref(),
                Some("1.2.3.4"),
                "{field:?} must carry (0000,1001) Requested SOP Instance UID"
            );
            assert_eq!(
                element_value(&bytes, 0x0000, 0x0002),
                None,
                "{field:?} must not carry Affected SOP Class UID"
            );
            assert_eq!(
                element_value(&bytes, 0x0000, 0x1000),
                None,
                "{field:?} must not carry Affected SOP Instance UID"
            );
        }
    }

    #[test]
    fn n_get_carries_the_attribute_identifier_list() {
        let mut request = NServiceRequest::new(CommandField::NGetRq, 1);
        request.sop_class_uid = Some("1.2.3".into());
        request.sop_instance_uid = Some("1.2.3.4".into());
        request.attribute_identifier_list = vec![
            radicom::model::tag::Tag::PATIENT_NAME,
            radicom::model::tag::Tag::PIXEL_DATA,
        ];

        let bytes = command_bytes(&request.to_command());
        let value = element_value(&bytes, 0x0000, 0x1005).expect("Attribute Identifier List");
        assert_eq!(value, vec![0x10, 0x00, 0x10, 0x00, 0xE0, 0x7F, 0x10, 0x00]);

        let parsed = Command::from_dataset(&request.to_command().to_dataset()).unwrap();
        let back = NServiceRequest::from_command(&parsed, None);
        assert_eq!(
            back.attribute_identifier_list,
            request.attribute_identifier_list
        );
    }

    #[test]
    fn affected_services_encode_the_affected_tags() {
        // N-CREATE and N-EVENT-REPORT act on an affected instance (PS3.7 §10.1)
        for field in [CommandField::NCreateRq, CommandField::NEventReportRq] {
            let mut request = NServiceRequest::new(field, 1);
            request.sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.88.1".into());
            request.sop_instance_uid = Some("1.2.3.4".into());
            let bytes = command_bytes(&request.to_command());
            assert_eq!(
                element_text(&bytes, 0x0000, 0x0002).as_deref(),
                Some("1.2.840.10008.5.1.4.1.1.88.1"),
                "{field:?} must carry (0000,0002) Affected SOP Class UID"
            );
            assert_eq!(
                element_text(&bytes, 0x0000, 0x1000).as_deref(),
                Some("1.2.3.4"),
                "{field:?} must carry (0000,1000) Affected SOP Instance UID"
            );
            assert_eq!(element_value(&bytes, 0x0000, 0x0003), None);
            assert_eq!(element_value(&bytes, 0x0000, 0x1001), None);
        }
    }

    #[test]
    fn a_parsed_request_reads_the_tags_its_field_uses() {
        // an N-GET-RQ as a peer encodes it: Requested* only
        let mut command = Command::request(CommandField::NGetRq, 5);
        command.requested_sop_class_uid = Some("1.2.3".into());
        command.requested_sop_instance_uid = Some("1.2.3.4".into());
        let parsed = Command::from_dataset(&command.to_dataset()).unwrap();
        let request = NServiceRequest::from_command(&parsed, None);
        assert_eq!(request.sop_class_uid.as_deref(), Some("1.2.3"));
        assert_eq!(request.sop_instance_uid.as_deref(), Some("1.2.3.4"));

        // an N-CREATE-RQ carries Affected* instead
        let mut command = Command::request(CommandField::NCreateRq, 6);
        command.affected_sop_class_uid = Some("1.2.3".into());
        command.affected_sop_instance_uid = Some("1.2.3.5".into());
        let parsed = Command::from_dataset(&command.to_dataset()).unwrap();
        let request = NServiceRequest::from_command(&parsed, None);
        assert_eq!(request.sop_class_uid.as_deref(), Some("1.2.3"));
        assert_eq!(request.sop_instance_uid.as_deref(), Some("1.2.3.5"));
    }

    #[test]
    fn a_success_response_maps_the_field_and_status() {
        let request = NServiceRequest::new(CommandField::NGetRq, 3);
        let response = NServiceResponse::success(&request, None);
        assert_eq!(response.field, CommandField::NGetRsp);
        assert_eq!(response.message_id_being_responded_to, 3);
        assert_eq!(response.status, status::SUCCESS);
    }

    #[test]
    fn a_response_echoes_the_instance_as_affected() {
        // PS3.7 §10.1: responses name the instance in (0000,0002)/(0000,1000)
        let mut request = NServiceRequest::new(CommandField::NGetRq, 3);
        request.sop_class_uid = Some("1.2.3".into());
        request.sop_instance_uid = Some("1.2.3.4".into());
        let bytes = command_bytes(&NServiceResponse::success(&request, None).to_command());
        assert_eq!(
            element_text(&bytes, 0x0000, 0x0002).as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            element_text(&bytes, 0x0000, 0x1000).as_deref(),
            Some("1.2.3.4")
        );
        assert_eq!(
            element_value(&bytes, 0x0000, 0x0003),
            None,
            "a response is not a request"
        );
    }

    #[test]
    fn an_n_create_success_can_return_the_new_instance_uid() {
        let mut request = NServiceRequest::new(CommandField::NCreateRq, 3);
        request.sop_class_uid = Some("1.2.3".into());
        let response = NServiceResponse::success_with_new_instance(&request, "1.2.3.4", None);
        let bytes = command_bytes(&response.to_command());

        assert_eq!(
            element_text(&bytes, 0x0000, 0x0002).as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            element_text(&bytes, 0x0000, 0x1000).as_deref(),
            Some("1.2.3.4")
        );
    }

    #[test]
    fn a_failure_response_keeps_the_status() {
        let request = NServiceRequest::new(CommandField::NDeleteRq, 9);
        let response = NServiceResponse::failure(&request, status::PROCESSING_FAILURE);
        assert_eq!(response.status, status::PROCESSING_FAILURE);
        assert!(response.dataset.is_none());
    }

    #[test]
    fn a_response_command_round_trips() {
        let request = NServiceRequest::new(CommandField::NCreateRq, 4);
        let response = NServiceResponse::success(&request, Some(Dataset::new()));
        let command = response.to_command();
        let parsed = Command::from_dataset(&command.to_dataset()).unwrap();
        let back = NServiceResponse::from_command(&parsed, None);
        assert_eq!(back.field, CommandField::NCreateRsp);
        assert_eq!(back.message_id_being_responded_to, 4);
        assert_eq!(back.status, status::SUCCESS);
        assert!(
            !parsed.no_dataset,
            "a response carrying a dataset clears the flag"
        );
    }
}
