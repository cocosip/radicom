//! Heterogeneous DIMSE request and response models.

use crate::cfind::{CFindRequest, CFindResponse};
use crate::cget::{CGetRequest, CGetResponse};
use crate::cmove::{CMoveRequest, CMoveResponse};
use crate::nservice::{NServiceRequest, NServiceResponse};
use crate::services::{CEchoRequest, CEchoResponse, CStoreRequest, CStoreResponse};
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
    /// A complete C-STORE DIMSE request, including command fields.
    StoreRequest(Box<CStoreRequest>),
    Find(CFindRequest),
    Get {
        request: CGetRequest,
        storage_sop_classes: Vec<String>,
    },
    Move(CMoveRequest),
    NService(NServiceRequest),
}

impl DicomRequest {
    /// Creates an outbound C-STORE request from a shared file handle.
    pub fn store(file: Arc<DicomFile>) -> Self {
        Self::Store { file }
    }

    /// Creates an outbound C-STORE request from the fo-dicom-shaped request
    /// object. Use this when Message ID or Move Originator fields matter.
    pub fn store_request(request: CStoreRequest) -> Self {
        Self::StoreRequest(Box::new(request))
    }

    /// Creates a C-GET request and declares the storage SOP classes that may
    /// arrive as reverse C-STORE sub-operations.
    pub fn get(
        request: CGetRequest,
        storage_sop_classes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self::Get {
            request,
            storage_sop_classes: storage_sop_classes.into_iter().map(Into::into).collect(),
        }
    }

    pub fn kind(&self) -> RequestKind {
        match self {
            Self::Echo(_) => RequestKind::Echo,
            Self::Store { .. } => RequestKind::Store,
            Self::StoreRequest(_) => RequestKind::Store,
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
            Self::StoreRequest(request) => (request.message_id != 0).then_some(request.message_id),
        }
    }
}

impl From<CEchoRequest> for DicomRequest {
    fn from(request: CEchoRequest) -> Self {
        Self::Echo(request)
    }
}

impl From<Arc<DicomFile>> for DicomRequest {
    fn from(file: Arc<DicomFile>) -> Self {
        Self::store(file)
    }
}

impl From<CStoreRequest> for DicomRequest {
    fn from(request: CStoreRequest) -> Self {
        Self::store_request(request)
    }
}

impl From<CFindRequest> for DicomRequest {
    fn from(request: CFindRequest) -> Self {
        Self::Find(request)
    }
}

impl From<CGetRequest> for DicomRequest {
    /// Queues a C-GET without additional storage contexts. Use [`DicomRequest::get`]
    /// when the request may receive reverse C-STORE sub-operations.
    fn from(request: CGetRequest) -> Self {
        Self::Get {
            request,
            storage_sop_classes: Vec::new(),
        }
    }
}

impl From<CMoveRequest> for DicomRequest {
    fn from(request: CMoveRequest) -> Self {
        Self::Move(request)
    }
}

impl From<NServiceRequest> for DicomRequest {
    fn from(request: NServiceRequest) -> Self {
        Self::NService(request)
    }
}

/// The stable position of a request in a [`DicomRequestQueue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestHandle(usize);

impl RequestHandle {
    /// Returns the zero-based position assigned when the request was queued.
    pub fn index(self) -> usize {
        self.0
    }
}

/// Ordered heterogeneous DIMSE requests sent on one Association.
#[derive(Debug, Default)]
pub struct DicomRequestQueue {
    requests: Vec<DicomRequest>,
}

impl DicomRequestQueue {
    /// Creates an empty request queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a request and returns its stable position in the queue.
    pub fn push(&mut self, request: impl Into<DicomRequest>) -> RequestHandle {
        let handle = RequestHandle(self.requests.len());
        self.requests.push(request.into());
        handle
    }

    /// Returns the number of queued requests.
    pub fn len(&self) -> usize {
        self.requests.len()
    }

    /// Returns whether the queue has no requests.
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    /// Consumes the queue for transmission by [`crate::client::DicomClient`].
    pub(crate) fn into_requests(self) -> Vec<DicomRequest> {
        self.requests
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

    /// Returns the queue handle for this response.
    pub fn request_handle(&self) -> RequestHandle {
        RequestHandle(self.request_index)
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
            DicomRequest::StoreRequest(Box::new(CStoreRequest {
                message_id: 1,
                affected_sop_class_uid: "1.2.3".into(),
                affected_sop_instance_uid: "1.2.3.4".into(),
                priority: crate::dimse::Priority::Medium,
                file: DicomFile::from_dataset(radicom::Dataset::new()),
                move_originator_ae_title: None,
                move_originator_message_id: None,
            }))
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
        assert_eq!(
            DicomRequest::StoreRequest(Box::new(CStoreRequest {
                message_id: 12,
                affected_sop_class_uid: "1.2.3".into(),
                affected_sop_instance_uid: "1.2.3.4".into(),
                priority: crate::dimse::Priority::Medium,
                file: DicomFile::from_dataset(radicom::Dataset::new()),
                move_originator_ae_title: None,
                move_originator_message_id: None,
            }))
            .message_id(),
            Some(12)
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

    #[test]
    fn request_queue_preserves_order_and_returns_handles() {
        let mut queue = DicomRequestQueue::new();
        let echo = queue.push(CEchoRequest {
            message_id: 1,
            affected_sop_class_uid: "1.2.840.10008.1.1".into(),
        });
        let find = queue.push(CFindRequest::study_query(2));

        assert_eq!(queue.len(), 2);
        assert!(!queue.is_empty());
        assert_eq!(echo.index(), 0);
        assert_eq!(find.index(), 1);
        let requests = queue.into_requests();
        assert!(matches!(requests[0], DicomRequest::Echo(_)));
        assert!(matches!(requests[1], DicomRequest::Find(_)));
    }

    #[test]
    fn request_queue_accepts_store_constructor_without_copying_file_handle() {
        let file = Arc::new(DicomFile::from_dataset(radicom::Dataset::new()));
        let mut queue = DicomRequestQueue::new();
        let handle = queue.push(DicomRequest::store(file.clone()));

        assert_eq!(handle.index(), 0);
        let requests = queue.into_requests();
        assert!(matches!(&requests[0], DicomRequest::Store { .. }));
        if let DicomRequest::Store { file: queued } = &requests[0] {
            assert!(Arc::ptr_eq(queued, &file));
        }
    }

    #[test]
    fn response_event_exposes_request_handle() {
        let event = DicomResponseEvent {
            request_index: 7,
            message_id: 9,
            response: DicomResponse::CEcho(CEchoResponse::success(&CEchoRequest {
                message_id: 9,
                affected_sop_class_uid: "1.2.840.10008.1.1".into(),
            })),
            is_final: true,
        };
        assert_eq!(event.request_handle().index(), 7);
    }

    #[test]
    fn request_conversions_cover_all_dimse_variants() {
        let file = Arc::new(DicomFile::from_dataset(radicom::Dataset::new()));
        assert!(matches!(
            DicomRequest::from(file),
            DicomRequest::Store { .. }
        ));
        let store = CStoreRequest {
            message_id: 1,
            affected_sop_class_uid: "1.2.3".into(),
            affected_sop_instance_uid: "1.2.3.4".into(),
            priority: crate::dimse::Priority::Medium,
            file: DicomFile::from_dataset(radicom::Dataset::new()),
            move_originator_ae_title: None,
            move_originator_message_id: None,
        };
        assert!(matches!(
            DicomRequest::from(store),
            DicomRequest::StoreRequest(_)
        ));
        assert!(matches!(
            DicomRequest::from(CGetRequest::study("1.2.3", 1)),
            DicomRequest::Get { .. }
        ));
        assert!(matches!(
            DicomRequest::get(CGetRequest::study("1.2.3", 1), ["1.2.840.10008.5.1.4.1.1.7"]),
            DicomRequest::Get {
                storage_sop_classes,
                ..
            } if storage_sop_classes == ["1.2.840.10008.5.1.4.1.1.7"]
        ));
        assert!(matches!(
            DicomRequest::from(CMoveRequest::study("1.2.3", "DEST", 1)),
            DicomRequest::Move(_)
        ));
        assert!(matches!(
            DicomRequest::from(NServiceRequest::new(crate::dimse::CommandField::NGetRq, 1)),
            DicomRequest::NService(_)
        ));
    }
}
