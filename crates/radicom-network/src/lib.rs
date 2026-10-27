//! radicom-network — DICOM network communication (DIMSE) on tokio.
//!
//! PDU codec, association negotiation, the per-connection protocol engine,
//! SCU/SCP services and the two-layer client. Implementation follows
//! milestones M5–M6 per docs/07-network.md.

pub mod assembler;
pub mod association;
pub mod dimse;
pub mod error;
pub mod pdu;

pub use assembler::{MessageUnit, PduSplitter, PdvAssembler, Stream};
pub use association::{
    AcceptedContexts, ContextSetBuilder, ContextState, NegotiatedContext, NegotiationPolicy, ProposedContext,
    SyntaxPriority,
};
pub use dimse::{Command, CommandField, Priority, Status, StatusState};
pub use error::{AbortSource, NetworkError, RejectResult, RejectSource, Result};
pub use pdu::{
    Abort, Associate, AssociateRj, AsyncOperations, CommonExtended, ExtendedNegotiation, PData, Pdu, PduType, Pdv,
    PresentationContext, RoleSelection, UserIdentity, UserInformation, APPLICATION_CONTEXT, MAX_PDU_SIZE,
};
