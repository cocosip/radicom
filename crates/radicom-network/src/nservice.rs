//! N-service message types (SRV-07; docs/07-network.md §7.6).
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

/// An N-service request (all six services).
#[derive(Debug, Clone)]
pub struct NServiceRequest {
    pub field: CommandField,
    pub message_id: u16,
    pub affected_sop_class_uid: Option<String>,
    pub affected_sop_instance_uid: Option<String>,
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
            affected_sop_class_uid: None,
            affected_sop_instance_uid: None,
            event_type_id: None,
            action_type_id: None,
            dataset: None,
        }
    }

    /// Builds the wire command (the dataset, if any, is sent separately).
    pub fn to_command(&self) -> Command {
        let mut command = Command::request(self.field, self.message_id);
        command.affected_sop_class_uid = self.affected_sop_class_uid.clone();
        command.affected_sop_instance_uid = self.affected_sop_instance_uid.clone();
        command.event_type_id = self.event_type_id;
        command.action_type_id = self.action_type_id;
        command.no_dataset = self.dataset.is_none();
        command
    }

    /// Parses a request from a received command plus its (optional) dataset.
    pub fn from_command(command: &Command, dataset: Option<Dataset>) -> Self {
        NServiceRequest {
            field: command.field,
            message_id: command.message_id,
            affected_sop_class_uid: command.affected_sop_class_uid.clone(),
            affected_sop_instance_uid: command.affected_sop_instance_uid.clone(),
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

    /// A failure response to `request` with `status`.
    pub fn failure(request: &NServiceRequest, status: u16) -> Self {
        NServiceResponse::from_request(request, status)
    }

    fn from_request(request: &NServiceRequest, status: u16) -> Self {
        NServiceResponse {
            field: crate::dimse::response_field(request.field)
                .unwrap_or(request.field),
            message_id_being_responded_to: request.message_id,
            affected_sop_class_uid: request.affected_sop_class_uid.clone(),
            affected_sop_instance_uid: request.affected_sop_instance_uid.clone(),
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
            status: command.status.unwrap_or(crate::dimse::status::SUCCESS),
            dataset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dimse::status;

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
        request.affected_sop_class_uid = Some("1.2.3".into());
        request.affected_sop_instance_uid = Some("1.2.3.4".into());
        request.action_type_id = Some(1);
        let command = request.to_command();
        let dataset = command.to_dataset();
        let parsed = Command::from_dataset(&dataset).unwrap();
        let back = NServiceRequest::from_command(&parsed, None);
        assert_eq!(back.field, CommandField::NActionRq);
        assert_eq!(back.message_id, 7);
        assert_eq!(back.affected_sop_instance_uid.as_deref(), Some("1.2.3.4"));
        assert_eq!(back.action_type_id, Some(1));
        assert!(back.dataset.is_none());
        assert!(parsed.no_dataset, "a request without a dataset sets the flag");
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
        assert!(!parsed.no_dataset, "a response carrying a dataset clears the flag");
    }
}
