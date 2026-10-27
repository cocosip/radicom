//! radicom-network — DICOM network communication (DIMSE) on tokio.
//!
//! PDU codec, association negotiation, the per-connection protocol engine,
//! SCU/SCP services and the two-layer client. Implementation follows
//! milestones M5–M6 per docs/07-network.md.

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
pub use dimse::{Command, CommandField, Priority, Status, StatusState};
pub use engine::{CircuitBreaker, ConnectionLifecycle, ConnectionState, EngineOptions, PendingRequest, PendingTable};
pub use error::{AbortSource, NetworkError, RejectResult, RejectSource, Result};
pub use expert::{send as send_expert, Connection, ConnectionEvent, ExpertRequest, ExpertResponse, ResponseStream};
pub use hosting::{HostedDicomServer, HostedServerConfig, ServerHealth};
pub use observability::{CountingMetrics, MetricsCollector, MetricsSnapshot, NoopMetrics};
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
