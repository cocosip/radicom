//! radicom-network — DICOM network communication (DIMSE) on tokio.
//!
//! PDU codec, association negotiation, the per-connection protocol engine,
//! SCU/SCP services and the two-layer client. Implementation follows
//! milestones M5–M6 per docs/01-architecture.md §11.

pub mod assembler;
pub mod association;
pub mod cfind;
pub mod cget;
pub mod client;
pub mod cmove;
pub mod dimse;
pub mod engine;
pub mod error;
pub mod expert;
pub mod extended;
pub mod hosting;
pub mod message;
pub mod nservice;
pub mod observability;
pub mod pdu;
pub mod registry;
pub mod print_sop;
pub mod server;
pub mod services;
pub mod store;
pub mod tls;
pub mod transport;

pub use assembler::{MessageUnit, PduSplitter, PdvAssembler, Stream};
pub use association::{
    AcceptedContexts, ContextSetBuilder, ContextState, NegotiatedContext, NegotiationPolicy, ProposedContext,
    SyntaxPriority,
};
pub use cfind::{CFindRequest, CFindResponse, QueryRetrieveLevel, QueryRoot, RetrieveService};
pub use cget::{CGetEvent, CGetRequest, CGetResponse};
pub use client::{ClientEvent, DicomClient, DicomClientOptions};
pub use cmove::{store_to_destination, CMoveRequest, CMoveResponse, SubOperationSummary};
pub use dimse::{
    status_description, status_description_for_service_class, Command, CommandField, Priority, Status,
    StatusServiceClass, StatusState,
};
pub use engine::{CircuitBreaker, ConnectionLifecycle, ConnectionState, EngineOptions, PendingRequest, PendingTable};
pub use error::{AbortSource, NetworkError, RejectResult, RejectSource, Result};
pub use expert::{send as send_expert, Connection, ConnectionEvent, ExpertRequest, ExpertResponse, ResponseStream};
pub use hosting::{HostedDicomServer, HostedServerConfig, ServerHealth};
pub use observability::{CountingMetrics, MetricsCollector, MetricsSnapshot, NoopMetrics};
pub use registry::{DicomServerRegistration, DicomServerRegistry};
#[cfg(feature = "otel")]
pub use observability::TracingMetrics;
pub use nservice::{is_n_request, NServiceRequest, NServiceResponse, N_REQUEST_FIELDS};
pub use extended::{
    apply_answer, validate_common_extended, AppInfo, ExtendedNegotiationPolicy, FindAppInfo, RetrieveAppInfo,
    StorageAppInfo, StorageOption,
};
pub use pdu::{
    Abort, Associate, AssociateRj, AsyncOperations, CommonExtended, ExtendedNegotiation, PData, Pdu, PduType, Pdv,
    PresentationContext, RoleSelection, UserIdentity, UserInformation, APPLICATION_CONTEXT, MAX_PDU_SIZE,
};
pub use print_sop::{
    network_identifier_from_mac, parse_mac, try_get_network_identifier, MetaSopClasses,
};

/// Test-only helpers for asserting wire bytes (not part of the public API).
#[cfg(test)]
pub(crate) mod test_wire {
    use crate::dimse::Command;

    /// Encodes a command set as it travels on the wire (implicit VR LE).
    pub(crate) fn command_bytes(command: &Command) -> Vec<u8> {
        crate::server::command_bytes(command).expect("encode command set")
    }

    /// The value of the implicit-VR element `(group, element)`, when present.
    pub(crate) fn element_value(bytes: &[u8], group: u16, element: u16) -> Option<Vec<u8>> {
        let key = [group.to_le_bytes(), element.to_le_bytes()].concat();
        let mut offset = 0usize;
        while offset + 8 <= bytes.len() {
            let len = u32::from_le_bytes([
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
                bytes[offset + 7],
            ]) as usize;
            if bytes.get(offset..offset + 4) == Some(&key[..]) {
                return bytes.get(offset + 8..offset + 8 + len).map(<[u8]>::to_vec);
            }
            offset += 8 + len;
        }
        None
    }

    /// The text of a string element, with the even-length pad removed.
    pub(crate) fn element_text(bytes: &[u8], group: u16, element: u16) -> Option<String> {
        element_value(bytes, group, element)
            .map(|value| String::from_utf8_lossy(&value).trim_end_matches('\0').to_string())
    }

    /// Answers a peer's A-ASSOCIATE-RQ with an AC that accepts every offered
    /// context (first syntax each), so a test can drive the ACSE phase itself.
    pub(crate) async fn accept_association(stream: &mut tokio::net::TcpStream) -> crate::pdu::Associate {
        let read = crate::transport::read_pdu(stream).await.expect("read A-ASSOCIATE-RQ");
        let Some(crate::pdu::Pdu::AssociateRq(request)) = read else {
            panic!("expected an A-ASSOCIATE-RQ");
        };
        let mut accept = request.clone();
        for context in &mut accept.presentation_contexts {
            context.result = 0;
            context.transfer_syntaxes.truncate(1);
        }
        crate::transport::write_pdu(stream, &crate::pdu::Pdu::AssociateAc(accept))
            .await
            .expect("write A-ASSOCIATE-AC");
        request
    }
}
