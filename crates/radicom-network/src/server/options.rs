//! Server and client option structs.

use super::*;

/// Server options (§11.4).
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Called AE title accepted by the SCP; `None` accepts any title.
    pub called_ae_title: Option<String>,
    /// 0 = unlimited.
    pub max_clients: usize,
    /// Peer maximum PDU length advertised in the association accept.
    pub max_pdu_length: u32,
    /// How many PDVs may share one P-DATA-TF this SCP writes (L7).
    ///
    /// fo-dicom `DicomServiceOptions.MaxPDVsPerPDU`; the default 16 is the
    /// interop workaround knob for peers that mishandle batched PDVs.
    pub max_pdvs_per_pdu: usize,
    /// Optional cap on one command PDV value this SCP writes (L7, fo-dicom
    /// `MaxCommandBuffer`). `None` fills the negotiated PDU.
    pub max_command_buffer: Option<u32>,
    /// Optional cap on one dataset PDV value this SCP writes (L7, fo-dicom
    /// `MaxDataBuffer`). `None` fills the negotiated PDU.
    pub max_data_buffer: Option<u32>,
    /// Maximum simultaneous operations this endpoint invokes/performs. Zero
    /// requests unlimited; accepted server windows and echo batches cap at 64.
    pub async_operations: crate::pdu::AsyncOperations,
    /// Transfer syntaxes accepted for every supported SOP class. Empty uses
    /// the two uncompressed little-endian syntaxes for compatibility.
    pub supported_transfer_syntaxes: Vec<String>,
    /// Whether offered order or this server's order wins during negotiation.
    pub syntax_priority: crate::association::SyntaxPriority,
    /// How long to wait for the peer's A-ASSOCIATE-RQ.
    pub association_timeout: Duration,
    /// How long the TLS handshake may take before the connection is dropped.
    /// It starts before the association timeout, while the client slot is
    /// already held, so a silent peer cannot occupy it forever (review #14).
    #[cfg(feature = "tls")]
    pub tls_handshake_timeout: Duration,
    /// Socket options applied to every accepted connection.
    pub tcp: TcpOptions,
    /// Where incoming C-STORE instances are put before the provider sees them.
    pub store_sink: crate::store::StoreSink,
    /// How a received C-STORE dataset is streamed to disk before it is parsed
    /// (H3). Memory is bounded by the PDU size, not by the instance size.
    pub store_spool: crate::assembler::SpoolConfig,
    /// Send a dataset whose syntax cannot be transcoded as-is, in its own
    /// source syntax, instead of failing the sub-operation (L3, fo-dicom
    /// `IgnoreUnsupportedTransferSyntaxChange`).
    ///
    /// Default `false`: a C-GET whose instance syntax no registered codec can
    /// convert fails that sub-operation, which is the safe answer. With `true`
    /// the bytes go out unchanged under the negotiated presentation context —
    /// PixelData is never stripped — and the peer decides what to do with a
    /// dataset in a syntax it did not negotiate. That is what fo-dicom's flag
    /// is for: interoperating with peers that mislabel their syntax.
    pub ignore_unsupported_syntax_change: bool,
    /// Fallback character set for received text that carries no (0008,0005)
    /// (H1), as a DICOM Defined Term such as `"ISO_IR 100"` (Latin-1).
    ///
    /// Applies to every dataset this SCP parses (C-STORE instances, C-FIND /
    /// C-MOVE / C-GET identifiers, and N-service requests). `None` keeps the
    /// default repertoire (ISO-IR 6), which is what a peer that omits the
    /// element declares. An unknown term is ignored rather than guessed.
    pub fallback_encoding: Option<&'static str>,
    /// How long an SCP-issued request waits for the peer's response (H2/M8).
    ///
    /// Applies to reverse C-STORE sub-operations and to N-EVENT-REPORT requests
    /// sent on this association; `None` keeps the wait unbounded.
    pub request_timeout: Option<Duration>,
    /// Answers SOP Class Extended Negotiation requests (§11.2).
    pub extended_negotiation: crate::extended::ExtendedNegotiationPolicy,
    /// User identity types this SCP accepts, and the response to echo (§11.2).
    pub user_identity: UserIdentityPolicy,
    /// TLS material; `None` serves plain TCP (NET-01, `tls` feature).
    #[cfg(feature = "tls")]
    pub tls: Option<crate::tls::TlsOptions>,
    /// Optional metrics collector (OBS-01); `None` disables instrumentation.
    pub metrics: Option<std::sync::Arc<dyn crate::observability::MetricsCollector>>,
}

/// A snapshot handle for the number of accepted connections that are still
/// being served by an SCP.
#[derive(Clone, Debug)]
pub struct ConnectedClients {
    pub count: Arc<AtomicUsize>,
}

impl ConnectedClients {
    /// Returns the current number of live connection tasks.
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}

pub struct ConnectionCountGuard {
    pub count: Arc<AtomicUsize>,
}

impl Drop for ConnectionCountGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::Release);
    }
}

/// How an SCP handles User Identity negotiation (§11.2).
#[derive(Debug, Clone, Default)]
pub enum UserIdentityPolicy {
    /// Reject the association when a user identity is requested.
    #[default]
    Reject,
    /// Accept any well-formed identity. When the request asked for a positive
    /// response, the AC carries an empty one (PS3.8 §D.3.3.7 requires it).
    Accept,
    /// Accept and echo this response byte string back (type 1–5 payload).
    Answer(Vec<u8>),
}

impl UserIdentityPolicy {
    /// The answer to `identity`, or `None` to reject the association.
    pub fn answer(&self, identity: &crate::pdu::UserIdentity) -> Option<Option<Vec<u8>>> {
        match self {
            UserIdentityPolicy::Reject => None,
            // PS3.8 §D.3.3.7: a requested positive response must be returned;
            // the empty response acknowledges the identity without comment.
            UserIdentityPolicy::Accept => Some(identity.positive_response_requested.then(Vec::new)),
            UserIdentityPolicy::Answer(response) => {
                // a response only means something when the peer asked for one
                if identity.positive_response_requested {
                    Some(Some(response.clone()))
                } else {
                    Some(None)
                }
            }
        }
    }
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            called_ae_title: None,
            max_clients: 0,
            max_pdu_length: 16384,
            max_pdvs_per_pdu: 16,
            max_command_buffer: None,
            max_data_buffer: None,
            async_operations: crate::pdu::AsyncOperations::default(),
            supported_transfer_syntaxes: vec![
                syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.uid().to_string(),
                syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.uid().to_string(),
            ],
            syntax_priority: crate::association::SyntaxPriority::Offered,
            association_timeout: Duration::from_secs(10),
            #[cfg(feature = "tls")]
            tls_handshake_timeout: Duration::from_secs(10),
            tcp: TcpOptions::default(),
            store_sink: crate::store::StoreSink::default(),
            store_spool: crate::assembler::SpoolConfig::default(),
            ignore_unsupported_syntax_change: false,
            fallback_encoding: None,
            request_timeout: None,
            extended_negotiation: crate::extended::ExtendedNegotiationPolicy::decline_all(),
            user_identity: UserIdentityPolicy::default(),
            #[cfg(feature = "tls")]
            tls: None,
            metrics: None,
        }
    }
}

/// Client options (§11.4, reduced to what M5/M6 need).
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub max_pdu_length: u32,
    /// How many PDVs may share one P-DATA-TF this side writes (L7, fo-dicom
    /// `MaxPDVsPerPDU`).
    pub max_pdvs_per_pdu: usize,
    /// Optional cap on one command PDV value this side writes (L7, fo-dicom
    /// `MaxCommandBuffer`). `None` fills the negotiated PDU.
    pub max_command_buffer: Option<u32>,
    /// Optional cap on one dataset PDV value this side writes (L7, fo-dicom
    /// `MaxDataBuffer`). `None` fills the negotiated PDU.
    pub max_data_buffer: Option<u32>,
    /// Maximum simultaneous operations this endpoint invokes/performs. Zero
    /// requests unlimited; accepted server windows and echo batches cap at 64.
    pub async_operations: crate::pdu::AsyncOperations,
    pub association_timeout: Duration,
    /// How long the TCP connect may take before the attempt fails (M7).
    ///
    /// `None` keeps the operating system's default, which can be minutes on a
    /// host that silently drops SYNs.
    pub connect_timeout: Option<Duration>,
    /// How long the TLS handshake may take before the connection attempt fails
    /// (review #14).
    #[cfg(feature = "tls")]
    pub tls_handshake_timeout: Duration,
    /// Maximum quiet time while waiting for a response to one DIMSE request.
    /// `None` keeps the request wait unbounded.
    pub request_timeout: Option<Duration>,
    /// Socket options applied to the connected stream.
    pub tcp: TcpOptions,
    /// How long a graceful release waits for the peer's A-RELEASE-RP before
    /// the association is reported as timed out (§9.3.6).
    pub release_timeout: Duration,
    /// User identity to offer; validated before the association is attempted
    /// (§11.2).
    pub user_identity: Option<crate::pdu::UserIdentity>,
    /// SOP Class Extended Negotiation items to offer (§11.2).
    pub extended_negotiations: Vec<crate::pdu::ExtendedNegotiation>,
    /// Send a dataset whose syntax cannot be transcoded as-is, in its own
    /// source syntax (L3, fo-dicom `IgnoreUnsupportedTransferSyntaxChange`).
    ///
    /// The SCU-side counterpart of [`ServerOptions::ignore_unsupported_syntax_change`];
    /// a C-MOVE SCP forwards its own setting here when it pushes to a
    /// destination.
    pub ignore_unsupported_syntax_change: bool,
    /// Fallback character set for *received* text that carries no (0008,0005),
    /// as a DICOM Defined Term such as `"ISO_IR 192"` (UTF-8).
    ///
    /// Applies to the datasets this SCU parses: reverse C-STORE instances from a
    /// C-GET, C-FIND/C-MOVE/C-GET response identifiers, and N-service response
    /// datasets. `None` keeps the default repertoire (ISO-IR 6). Datasets this
    /// side sends are never re-decoded: the writer uses the dataset's own
    /// fallback list.
    pub fallback_encoding: Option<&'static str>,
    /// SOP Class Common Extended Negotiation to offer (L4, PS3.7 §D.3.3.6).
    ///
    /// The item names a service class and its related general SOP classes, so
    /// an SCP can recognise a familiar family under a new class UID.
    pub common_extended: Option<crate::pdu::CommonExtended>,
    /// Role selection items to propose (§11.2).
    ///
    /// A peer may only send C-STORE requests to this side when the SCP role for
    /// that storage class was proposed here and granted in the accept
    /// (review #16).
    pub role_selections: Vec<crate::pdu::RoleSelection>,
    /// TLS material; `None` connects over plain TCP (NET-01, `tls` feature).
    #[cfg(feature = "tls")]
    pub tls: Option<crate::tls::TlsOptions>,
    /// Optional metrics collector (L6, fo-dicom `DicomServiceOptions` metrics).
    ///
    /// Counts bytes, connections and associations for this SCU's services, the
    /// same instrumentation the SCP already had.
    pub metrics: Option<std::sync::Arc<dyn crate::observability::MetricsCollector>>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            max_pdu_length: 16384,
            max_pdvs_per_pdu: 16,
            max_command_buffer: None,
            max_data_buffer: None,
            async_operations: crate::pdu::AsyncOperations::default(),
            association_timeout: Duration::from_secs(5),
            connect_timeout: None,
            #[cfg(feature = "tls")]
            tls_handshake_timeout: Duration::from_secs(10),
            request_timeout: None,
            release_timeout: Duration::from_secs(10),
            tcp: TcpOptions::default(),
            user_identity: None,
            extended_negotiations: Vec::new(),
            ignore_unsupported_syntax_change: false,
            fallback_encoding: None,
            common_extended: None,
            role_selections: Vec::new(),
            #[cfg(feature = "tls")]
            tls: None,
            metrics: None,
        }
    }
}
