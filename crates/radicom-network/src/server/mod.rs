//! End-to-end association and C-ECHO over a real TCP socket
//! (SRV-01/02; docs/architecture.md §11).
//!
//! Split into submodules: `options` (option structs), `wire` (command-set
//! codecs), `scp` (server engine), `scu` (client association).

mod options;
mod scp;
mod scu;
#[cfg(test)]
mod tests;
mod wire;

pub(crate) use crate::assembler::{MessageBody, MessageUnit, PduSplitter, PdvAssembler, Stream};
pub(crate) use crate::association::{AcceptedContexts, NegotiationPolicy};
pub(crate) use crate::dimse::{Command, CommandField, StatusState};
pub(crate) use crate::error::{AbortSource, NetworkError, RejectResult, RejectSource, Result};
pub(crate) use crate::pdu::{
    Abort, Associate, AssociateRj, Pdu, PresentationContext, APPLICATION_CONTEXT,
};
pub(crate) use crate::services::{CEchoRequest, CEchoResponse, CStoreResponse, ServiceProvider};
pub(crate) use crate::transport::{read_pdu, write_pdu, write_pdus, TcpOptions};
pub(crate) use radicom::model::dataset::Dataset;
pub(crate) use radicom::model::tag::Tag;
pub(crate) use radicom::model::transfer_syntax::syntaxes;
pub(crate) use std::collections::VecDeque;
pub(crate) use std::sync::atomic::{AtomicUsize, Ordering};
pub(crate) use std::sync::Arc;
pub(crate) use std::time::Duration;
pub(crate) use tokio::net::{TcpListener, TcpStream};

/// Implementation Class UID written into user information (§11.2).
pub const IMPLEMENTATION_CLASS_UID: &str = "1.2.826.0.1.3680043.10.1330.2";

/// Implementation Version Name written into user information (§11.2). It is the
/// core crate's constant, which embeds the workspace version, so the file meta
/// and network identities always agree.
pub use radicom::io::file::IMPLEMENTATION_VERSION_NAME;

pub use options::{ClientOptions, ConnectedClients, ServerOptions, UserIdentityPolicy};
pub use scp::DicomServer;
pub use scu::{Association, NEventReportHandler, ReverseStoreHandler};
pub use wire::{command_bytes, command_from_bytes};
