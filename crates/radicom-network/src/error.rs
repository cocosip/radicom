//! Network error type (docs/01-architecture.md §11).
//!
//! Every failure mode of the protocol engine maps onto this set; nothing
//! escapes as a generic IO error when a specific variant applies.

use std::io;

/// Result alias for the network crate.
pub type Result<T> = std::result::Result<T, NetworkError>;

/// Association rejection reasons, split by source (§11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectResult {
    Permanent,
    Transient,
}

impl RejectResult {
    pub fn code(self) -> u8 {
        match self {
            RejectResult::Permanent => 1,
            RejectResult::Transient => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(RejectResult::Permanent),
            2 => Some(RejectResult::Transient),
            _ => None,
        }
    }
}

/// Who rejected the association.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectSource {
    /// 1 — the called AE (service user).
    User,
    /// 2 — the ACSE provider.
    ProviderAcse,
    /// 3 — the presentation provider.
    ProviderPresentation,
}

impl RejectSource {
    pub fn code(self) -> u8 {
        match self {
            RejectSource::User => 1,
            RejectSource::ProviderAcse => 2,
            RejectSource::ProviderPresentation => 3,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(RejectSource::User),
            2 => Some(RejectSource::ProviderAcse),
            3 => Some(RejectSource::ProviderPresentation),
            _ => None,
        }
    }
}

/// A-ABORT source (PS3.8 §9.3.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortSource {
    /// 0 — the service user (DUL user).
    User,
    /// 2 — the service provider.
    Provider,
}

impl AbortSource {
    pub fn code(self) -> u8 {
        match self {
            AbortSource::User => 0,
            AbortSource::Provider => 2,
        }
    }

    /// `None` for 1 (reserved) and for anything outside 0/2: a malformed abort
    /// must not decode as a valid source.
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(AbortSource::User),
            2 => Some(AbortSource::Provider),
            _ => None,
        }
    }
}

/// All network failures (docs/01-architecture.md §11).
#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("association rejected (result {result}, source {source_code}, reason {reason})")]
    Reject {
        result: u8,
        source_code: u8,
        reason: u8,
    },
    #[error("association aborted (source {source_code}, reason {reason})")]
    Abort { source_code: u8, reason: u8 },
    #[error("association request timed out after {attempts} attempt(s)")]
    AssociationRequestTimedOut { attempts: u32 },
    #[error("request {message_id} timed out")]
    RequestTimedOut { message_id: u16 },
    #[error("A-RELEASE-RP did not arrive within {timeout_ms} ms")]
    ReleaseTimedOut { timeout_ms: u64 },
    #[error("connection closed prematurely")]
    ClosedPrematurely { source: Option<Box<NetworkError>> },
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    /// The peer broke DIMSE by sending a response where a request belongs.
    ///
    /// The layer that noticed has already sent A-ABORT (PS3.8 §9.3.8), so the
    /// connection wrapper must not abort a second time (NET-26).
    #[error("DIMSE violation: {0}")]
    PeerViolation(&'static str),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// C-GET/C-MOVE sub-operation handlers must be registered up front.
    #[error("no reverse C-STORE handler registered (required for C-GET)")]
    ReverseStoreHandlerMissing,
    /// A request needs a presentation context the peer did not accept.
    #[error("SOP class {sop_class} is not supported by the association")]
    SopClassNotSupported { sop_class: String },
    /// The peer did not grant this side the SCU role for the class (§11.2).
    #[error("SOP class {sop_class}: the peer did not grant this side the SCU role")]
    ScuRoleNotNegotiated { sop_class: String },
    /// TLS setup failed (bad PEM, unusable key, handshake configuration).
    #[error("TLS error: {0}")]
    Tls(String),
    /// The TLS handshake did not finish inside `tls_handshake_timeout`.
    #[error("TLS handshake timed out after {timeout_ms} ms")]
    TlsHandshakeTimedOut { timeout_ms: u64 },
}

impl NetworkError {
    /// Wraps a protocol violation with a static message.
    pub fn protocol(message: &'static str) -> Self {
        NetworkError::Protocol(message)
    }

    /// Wraps a TLS setup failure.
    pub fn protocol_tls(message: impl Into<String>) -> Self {
        NetworkError::Tls(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_codes_round_trip() {
        assert_eq!(
            RejectResult::from_code(RejectResult::Permanent.code()),
            Some(RejectResult::Permanent)
        );
        assert_eq!(
            RejectResult::from_code(RejectResult::Transient.code()),
            Some(RejectResult::Transient)
        );
        assert_eq!(RejectResult::from_code(0), None);
        assert_eq!(RejectResult::from_code(3), None);

        for source in [
            RejectSource::User,
            RejectSource::ProviderAcse,
            RejectSource::ProviderPresentation,
        ] {
            assert_eq!(RejectSource::from_code(source.code()), Some(source));
        }
        assert_eq!(RejectSource::from_code(4), None);
    }

    #[test]
    fn abort_codes_cover_only_the_defined_sources() {
        assert_eq!(AbortSource::User.code(), 0);
        assert_eq!(AbortSource::Provider.code(), 2);
        for source in [AbortSource::User, AbortSource::Provider] {
            assert_eq!(AbortSource::from_code(source.code()), Some(source));
        }
        // PS3.8 §9.3.8: 1 is reserved, everything outside 0/2 is malformed
        assert_eq!(AbortSource::from_code(1), None, "1 is reserved");
        assert_eq!(AbortSource::from_code(9), None);
    }

    #[test]
    fn display_names_the_failure() {
        let err = NetworkError::RequestTimedOut { message_id: 7 };
        assert!(err.to_string().contains("request 7 timed out"), "got {err}");
        let err = NetworkError::protocol("bad PDU type");
        assert!(err.to_string().contains("bad PDU type"));
        // a peer violation is a distinct variant: only `Protocol` makes the
        // connection layer send its own A-ABORT (NET-26)
        let err = NetworkError::PeerViolation("peer sent a response");
        assert!(err.to_string().contains("peer sent a response"));
        assert!(!matches!(err, NetworkError::Protocol(_)));
    }

    #[test]
    fn io_errors_convert() {
        let err: NetworkError = io::Error::new(io::ErrorKind::UnexpectedEof, "short").into();
        assert!(matches!(err, NetworkError::Io(_)));
    }
}
