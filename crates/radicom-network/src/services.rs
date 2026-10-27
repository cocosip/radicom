//! Services: C-ECHO and C-STORE (SRV-02/03; docs/07-network.md §7.1, §7.2).
//!
//! Providers are the SCP-side hooks. The built-in `VerificationProvider`
//! answers C-ECHO; everything else is user code.

use crate::association::{sop_class, transfer_syntax};
use crate::dimse::{status, Command, CommandField, StatusState};
use crate::error::{NetworkError, Result};
use async_trait::async_trait;
use radicom::io::file::DicomFile;

/// A C-ECHO request (§7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CEchoRequest {
    pub message_id: u16,
    pub affected_sop_class_uid: String,
}

impl CEchoRequest {
    /// Builds the command for this request.
    pub fn to_command(&self) -> Command {
        let mut command = Command::request(CommandField::CEchoRq, self.message_id);
        command.affected_sop_class_uid = Some(self.affected_sop_class_uid.clone());
        // C-ECHO never carries a dataset, so (0000,0800) must say so
        command.no_dataset = true;
        command
    }

    /// Reads a request off the wire.
    pub fn from_command(command: &Command) -> Result<Self> {
        if command.field != CommandField::CEchoRq {
            return Err(NetworkError::protocol("not a C-ECHO request"));
        }
        Ok(CEchoRequest {
            message_id: command.message_id,
            affected_sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
        })
    }
}

/// A C-ECHO response (§7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CEchoResponse {
    pub message_id_being_responded_to: u16,
    pub status: u16,
    pub affected_sop_class_uid: String,
}

impl CEchoResponse {
    pub fn success(request: &CEchoRequest) -> Self {
        CEchoResponse {
            message_id_being_responded_to: request.message_id,
            status: status::SUCCESS,
            affected_sop_class_uid: request.affected_sop_class_uid.clone(),
        }
    }

    pub fn failure(request: &CEchoRequest, code: u16) -> Self {
        CEchoResponse {
            message_id_being_responded_to: request.message_id,
            status: code,
            affected_sop_class_uid: request.affected_sop_class_uid.clone(),
        }
    }

    pub fn to_command(&self) -> Command {
        let mut command = Command::request(CommandField::CEchoRsp, self.message_id_being_responded_to);
        command.message_id_being_responded_to = Some(self.message_id_being_responded_to);
        command.status = Some(self.status);
        command.affected_sop_class_uid = Some(self.affected_sop_class_uid.clone());
        command.no_dataset = true;
        command
    }

    pub fn from_command(command: &Command) -> Result<Self> {
        if command.field != CommandField::CEchoRsp {
            return Err(NetworkError::protocol("not a C-ECHO response"));
        }
        Ok(CEchoResponse {
            message_id_being_responded_to: command.message_id_being_responded_to.unwrap_or(0),
            status: command.status.unwrap_or(status::PROCESSING_FAILURE),
            affected_sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
        })
    }

    pub fn state(&self) -> StatusState {
        crate::dimse::classify(self.status)
    }
}

/// A C-STORE request (§7.2).
#[derive(Debug, Clone)]
pub struct CStoreRequest {
    pub message_id: u16,
    pub affected_sop_class_uid: String,
    pub affected_sop_instance_uid: String,
    pub priority: crate::dimse::Priority,
    pub file: DicomFile,
    /// Set when the SCP is receiving a C-MOVE sub-operation (§7.5).
    pub move_originator_ae_title: Option<String>,
    pub move_originator_message_id: Option<u16>,
}

impl CStoreRequest {
    /// The command for this request; the dataset travels separately.
    pub fn to_command(&self) -> Command {
        let mut command = Command::request(CommandField::CStoreRq, self.message_id);
        command.affected_sop_class_uid = Some(self.affected_sop_class_uid.clone());
        command.affected_sop_instance_uid = Some(self.affected_sop_instance_uid.clone());
        command.priority = self.priority;
        command.move_originator_ae_title = self.move_originator_ae_title.clone();
        command.move_originator_message_id = self.move_originator_message_id;
        command.no_dataset = false;
        command
    }

    /// The instance UID, falling back to the dataset when the command lacks it.
    pub fn instance_uid(&self) -> &str {
        &self.affected_sop_instance_uid
    }
}

/// A C-STORE response (§7.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CStoreResponse {
    pub message_id_being_responded_to: u16,
    pub status: u16,
    pub affected_sop_class_uid: String,
    pub affected_sop_instance_uid: String,
    pub error_comment: Option<String>,
}

impl CStoreResponse {
    pub fn success(request: &CStoreRequest) -> Self {
        CStoreResponse {
            message_id_being_responded_to: request.message_id,
            status: status::SUCCESS,
            affected_sop_class_uid: request.affected_sop_class_uid.clone(),
            affected_sop_instance_uid: request.affected_sop_instance_uid.clone(),
            error_comment: None,
        }
    }

    /// A failure whose comment is truncated to the LO limit (§7.2).
    pub fn failure(request: &CStoreRequest, code: u16, comment: impl Into<String>) -> Self {
        CStoreResponse {
            message_id_being_responded_to: request.message_id,
            status: code,
            affected_sop_class_uid: request.affected_sop_class_uid.clone(),
            affected_sop_instance_uid: request.affected_sop_instance_uid.clone(),
            error_comment: Some(crate::dimse::truncate_lo(&comment.into())),
        }
    }

    pub fn to_command(&self) -> Command {
        let mut command = Command::request(CommandField::CStoreRsp, self.message_id_being_responded_to);
        command.message_id_being_responded_to = Some(self.message_id_being_responded_to);
        command.status = Some(self.status);
        command.affected_sop_class_uid = Some(self.affected_sop_class_uid.clone());
        command.affected_sop_instance_uid = Some(self.affected_sop_instance_uid.clone());
        command.error_comment = self.error_comment.clone();
        command.no_dataset = true;
        command
    }

    pub fn from_command(command: &Command) -> Result<Self> {
        if command.field != CommandField::CStoreRsp {
            return Err(NetworkError::protocol("not a C-STORE response"));
        }
        Ok(CStoreResponse {
            message_id_being_responded_to: command.message_id_being_responded_to.unwrap_or(0),
            status: command.status.unwrap_or(status::PROCESSING_FAILURE),
            affected_sop_class_uid: command.affected_sop_class_uid.clone().unwrap_or_default(),
            affected_sop_instance_uid: command.affected_sop_instance_uid.clone().unwrap_or_default(),
            error_comment: command.error_comment.clone(),
        })
    }

    pub fn state(&self) -> StatusState {
        crate::dimse::classify(self.status)
    }
}

/// The SCP-side hook set (§7).
#[async_trait]
pub trait ServiceProvider: Send + Sync {
    /// Abstract syntaxes this provider serves; drives PC negotiation (§7.1).
    fn supported_sop_classes(&self) -> Vec<String>;

    /// Answers C-ECHO.
    async fn on_c_echo(&self, _request: &CEchoRequest) -> Result<CEchoResponse> {
        Err(NetworkError::protocol("C-ECHO is not supported by this provider"))
    }

    /// Handles a received instance.
    async fn on_c_store(&self, request: &CStoreRequest) -> Result<CStoreResponse> {
        let _ = request;
        Err(NetworkError::protocol("C-STORE is not supported by this provider"))
    }

    /// Answers a C-FIND with a stream of Pending items and one terminal
    /// response (§7.3). The default refuses, so a provider opts in explicitly.
    fn on_c_find<'a>(&'a self, request: &'a crate::cfind::CFindRequest) -> futures::stream::BoxStream<'a, Result<crate::cfind::CFindResponse>> {
        let _ = request;
        Box::pin(futures::stream::once(async {
            Err(NetworkError::protocol("C-FIND is not supported by this provider"))
        }))
    }
}

/// The built-in Verification SCP: answers C-ECHO with Success (§7.1).
#[derive(Debug, Clone, Default)]
pub struct VerificationProvider;

#[async_trait]
impl ServiceProvider for VerificationProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![sop_class::VERIFICATION.to_string()]
    }

    async fn on_c_echo(&self, request: &CEchoRequest) -> Result<CEchoResponse> {
        // accept only the Verification abstract syntax
        if request.affected_sop_class_uid != sop_class::VERIFICATION {
            return Ok(CEchoResponse::failure(request, status::SOP_CLASS_NOT_SUPPORTED));
        }
        Ok(CEchoResponse::success(request))
    }
}

/// Transfer syntaxes a C-STORE request offers (§4.2).
///
/// The instance's own syntax plus any additional ones, with Implicit VR LE
/// force-included unless the caller opts out.
pub fn c_store_transfer_syntaxes(
    source_syntax: &str,
    additional: &[String],
    omit_implicit_vr: bool,
) -> Vec<String> {
    let mut out = Vec::with_capacity(additional.len() + 2);
    out.push(source_syntax.to_string());
    for syntax in additional {
        if !out.iter().any(|s| s == syntax) {
            out.push(syntax.clone());
        }
    }
    if !omit_implicit_vr && !out.iter().any(|s| s == transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN) {
        out.push(transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.to_string());
    }
    // a deflated source also offers plain explicit VR LE
    if source_syntax == transfer_syntax::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN
        && !out.iter().any(|s| s == transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN)
    {
        out.push(transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use radicom::model::dataset::Dataset;
    use radicom::model::tag::Tag;
    use radicom::model::transfer_syntax::syntaxes;
    use radicom::model::vr::Vr;

    fn sample_file() -> DicomFile {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::new(0x0008, 0x0016), Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::new(0x0008, 0x0018), Vr::Ui, "1.2.3.4.5");
        dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        DicomFile::from_dataset(dataset)
    }

    #[test]
    fn echo_request_round_trips_through_a_command() {
        let request = CEchoRequest {
            message_id: 3,
            affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
        };
        let command = request.to_command();
        assert_eq!(command.field, CommandField::CEchoRq);
        assert_eq!(CEchoRequest::from_command(&command).unwrap(), request);
    }

    #[test]
    fn echo_response_round_trips_through_a_command() {
        let request = CEchoRequest {
            message_id: 3,
            affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
        };
        let response = CEchoResponse::success(&request);
        let command = response.to_command();
        assert_eq!(command.field, CommandField::CEchoRsp);
        assert_eq!(command.message_id_being_responded_to, Some(3));
        assert!(command.no_dataset, "a C-ECHO response carries no dataset");
        assert_eq!(CEchoResponse::from_command(&command).unwrap(), response);
    }

    #[test]
    fn wrong_command_fields_are_rejected() {
        let command = Command::request(CommandField::CFindRq, 1);
        assert!(CEchoRequest::from_command(&command).is_err());
        assert!(CEchoResponse::from_command(&command).is_err());
        assert!(CStoreResponse::from_command(&command).is_err());
    }

    #[tokio::test]
    async fn verification_provider_accepts_verification_only() {
        let provider = VerificationProvider;
        assert_eq!(provider.supported_sop_classes(), vec![sop_class::VERIFICATION.to_string()]);

        let request = CEchoRequest {
            message_id: 1,
            affected_sop_class_uid: sop_class::VERIFICATION.to_string(),
        };
        let response = provider.on_c_echo(&request).await.unwrap();
        assert_eq!(response.status, status::SUCCESS);
        assert_eq!(response.state(), StatusState::Success);

        let wrong = CEchoRequest {
            message_id: 2,
            affected_sop_class_uid: "9.9.9".to_string(),
        };
        let response = provider.on_c_echo(&wrong).await.unwrap();
        assert_eq!(response.status, status::SOP_CLASS_NOT_SUPPORTED);
        assert_eq!(response.state(), StatusState::Failure);
    }

    #[tokio::test]
    async fn verification_provider_does_not_implement_c_store() {
        let provider = VerificationProvider;
        let request = CStoreRequest {
            message_id: 1,
            affected_sop_class_uid: "1.2.840.10008.5.1.4.1.1.7".into(),
            affected_sop_instance_uid: "1.2.3".into(),
            priority: crate::dimse::Priority::High,
            file: sample_file(),
            move_originator_ae_title: None,
            move_originator_message_id: None,
        };
        assert!(provider.on_c_store(&request).await.is_err(), "C-STORE is not part of Verification");
    }

    #[test]
    fn c_store_request_carries_the_move_originator() {
        let request = CStoreRequest {
            message_id: 9,
            affected_sop_class_uid: "1.2.840.10008.5.1.4.1.1.7".into(),
            affected_sop_instance_uid: "1.2.3".into(),
            priority: crate::dimse::Priority::Medium,
            file: sample_file(),
            move_originator_ae_title: Some("ORIGIN".into()),
            move_originator_message_id: Some(42),
        };
        let command = request.to_command();
        assert_eq!(command.move_originator_ae_title.as_deref(), Some("ORIGIN"));
        assert_eq!(command.move_originator_message_id, Some(42));
        assert_eq!(command.priority, crate::dimse::Priority::Medium);
        assert!(!command.no_dataset, "a C-STORE request carries a dataset");
    }

    #[test]
    fn c_store_response_round_trips_and_truncates_comments() {
        let request = CStoreRequest {
            message_id: 4,
            affected_sop_class_uid: "1.2.840.10008.5.1.4.1.1.7".into(),
            affected_sop_instance_uid: "1.2.3".into(),
            priority: crate::dimse::Priority::High,
            file: sample_file(),
            move_originator_ae_title: None,
            move_originator_message_id: None,
        };
        let ok = CStoreResponse::success(&request);
        assert_eq!(CStoreResponse::from_command(&ok.to_command()).unwrap(), ok);

        let failed = CStoreResponse::failure(&request, status::STORAGE_FAILURE, "x".repeat(200));
        let comment = failed.error_comment.as_deref().unwrap();
        assert_eq!(comment.len(), 64, "error comments are truncated to LO");
        assert_eq!(failed.state(), StatusState::Failure);
    }

    #[test]
    fn c_store_offers_the_source_syntax_plus_implicit() {
        let syntaxes = c_store_transfer_syntaxes(transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN, &[], false);
        assert_eq!(
            syntaxes,
            vec![
                transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string(),
                transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.to_string()
            ]
        );
    }

    #[test]
    fn c_store_can_omit_implicit_vr() {
        let syntaxes = c_store_transfer_syntaxes(transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN, &[], true);
        assert_eq!(syntaxes, vec![transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string()]);
    }

    #[test]
    fn deflated_sources_also_offer_explicit_vr_le() {
        let syntaxes = c_store_transfer_syntaxes(transfer_syntax::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN, &[], false);
        assert!(syntaxes.contains(&transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string()));
        assert_eq!(syntaxes[0], transfer_syntax::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN, "own syntax first");
    }

    #[test]
    fn additional_syntaxes_are_deduplicated() {
        let additional = vec![transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string()];
        let syntaxes = c_store_transfer_syntaxes(transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN, &additional, false);
        assert_eq!(
            syntaxes.iter().filter(|s| *s == transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN).count(),
            1,
            "no duplicates"
        );
    }
}
