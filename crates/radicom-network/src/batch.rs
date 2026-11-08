//! Heterogeneous DIMSE request and response models.

use crate::cfind::{CFindRequest, CFindResponse};
use crate::cget::{CGetRequest, CGetResponse};
use crate::cmove::{CMoveRequest, CMoveResponse};
use crate::nservice::{NServiceRequest, NServiceResponse};
use crate::services::{CEchoRequest, CEchoResponse, CStoreResponse};
use radicom::io::file::DicomFile;
use std::sync::Arc;

/// The DIMSE family represented by a queued request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Echo,
    Store,
    Find,
    Get,
    Move,
    NService,
}

/// An owned heterogeneous request for one association queue.
#[derive(Debug, Clone)]
pub enum DicomRequest {
    Echo(CEchoRequest),
    Store {
        file: Arc<DicomFile>,
    },
    Find(CFindRequest),
    Get {
        request: CGetRequest,
        storage_sop_classes: Vec<String>,
    },
    Move(CMoveRequest),
    NService(NServiceRequest),
}

impl DicomRequest {
    pub fn kind(&self) -> RequestKind {
        match self {
            Self::Echo(_) => RequestKind::Echo,
            Self::Store { .. } => RequestKind::Store,
            Self::Find(_) => RequestKind::Find,
            Self::Get { .. } => RequestKind::Get,
            Self::Move(_) => RequestKind::Move,
            Self::NService(_) => RequestKind::NService,
        }
    }

    /// Returns the caller-supplied Message ID, when this request has one.
    pub fn message_id(&self) -> Option<u16> {
        match self {
            Self::Echo(request) => (request.message_id != 0).then_some(request.message_id),
            Self::Find(request) => (request.message_id != 0).then_some(request.message_id),
            Self::Get { request, .. } => (request.message_id != 0).then_some(request.message_id),
            Self::Move(request) => (request.message_id != 0).then_some(request.message_id),
            Self::NService(request) => (request.message_id != 0).then_some(request.message_id),
            Self::Store { .. } => None,
        }
    }
}

/// One response emitted by the mixed request scheduler.
#[derive(Debug, Clone)]
pub enum DicomResponse {
    CEcho(CEchoResponse),
    CStore(CStoreResponse),
    CFind(CFindResponse),
    CGet(CGetResponse),
    CMove(CMoveResponse),
    NService(NServiceResponse),
}

/// A response associated with its queued request.
#[derive(Debug, Clone)]
pub struct DicomResponseEvent {
    pub request_index: usize,
    pub message_id: u16,
    pub response: DicomResponse,
    pub is_final: bool,
}

impl DicomResponseEvent {
    pub fn request_index(&self) -> usize {
        self.request_index
    }

    pub fn message_id(&self) -> u16 {
        self.message_id
    }

    pub fn is_final(&self) -> bool {
        self.is_final
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radicom::io::file::DicomFile;
    use std::sync::Arc;

    #[test]
    fn request_kind_identifies_each_dimse_family() {
        assert_eq!(
            DicomRequest::Echo(CEchoRequest {
                message_id: 0,
                affected_sop_class_uid: "1.2.3".into(),
            })
            .kind(),
            RequestKind::Echo
        );
        assert_eq!(
            DicomRequest::Store {
                file: Arc::new(DicomFile::from_dataset(radicom::Dataset::new()))
            }
            .kind(),
            RequestKind::Store
        );
        assert_eq!(
            DicomRequest::Find(crate::cfind::CFindRequest::study_query(1)).kind(),
            RequestKind::Find
        );
        assert_eq!(
            DicomRequest::Move(crate::cmove::CMoveRequest::study("1.2.3", "DEST", 2)).kind(),
            RequestKind::Move
        );
        assert_eq!(
            DicomRequest::Get {
                request: crate::cget::CGetRequest::study("1.2.3", 3),
                storage_sop_classes: Vec::new()
            }
            .kind(),
            RequestKind::Get
        );
        assert_eq!(
            DicomRequest::NService(crate::nservice::NServiceRequest::new(
                crate::dimse::CommandField::NGetRq,
                4
            ))
            .kind(),
            RequestKind::NService
        );
    }

    #[test]
    fn response_event_exposes_terminal_state() {
        let event = DicomResponseEvent {
            request_index: 2,
            message_id: 9,
            response: DicomResponse::CFind(crate::cfind::CFindResponse::success(
                &crate::cfind::CFindRequest::study_query(9),
                0,
            )),
            is_final: true,
        };
        assert!(event.is_final());
        assert_eq!(event.request_index(), 2);
        assert_eq!(event.message_id(), 9);
    }

    #[test]
    fn request_message_id_is_reported_for_explicit_requests() {
        let request = DicomRequest::Echo(crate::services::CEchoRequest {
            message_id: 12,
            affected_sop_class_uid: "1.2.3".into(),
        });
        assert_eq!(request.message_id(), Some(12));
        assert_eq!(
            DicomRequest::Store {
                file: Arc::new(DicomFile::from_dataset(radicom::Dataset::new()))
            }
            .message_id(),
            None
        );
    }

    #[test]
    fn store_request_owns_a_cloneable_file_handle() {
        let file = Arc::new(DicomFile::from_dataset(radicom::Dataset::new()));
        let request = DicomRequest::Store { file: file.clone() };
        let clone = request.clone();
        assert!(
            matches!(clone, DicomRequest::Store { file: cloned } if Arc::ptr_eq(&cloned, &file))
        );
    }
}
