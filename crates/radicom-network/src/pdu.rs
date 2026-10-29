//! PDU codec (NET-02; docs/01-architecture.md §11).
//!
//! Every PDU is `type(1) reserved(1) length(4, big endian)` followed by
//! `length` bytes of body. Decoding is total: any malformed input becomes a
//! typed error, never a panic (docs/01-architecture.md §11 fuzz requirement).

use crate::error::{AbortSource, NetworkError, RejectResult, RejectSource, Result};
use bytes::Bytes;

/// Upper bound for a whole received PDU (§3.1).
///
/// A peer may send a P-DATA-TF up to the Max-PDU-Length *we advertise*, so
/// [`clamp_max_pdu_length`] clamps every advertised value to this constant: the
/// read side then rejects only what we never promised to accept.
///
/// 256 KiB matches fo-dicom's default `DicomServiceOptions.MaxPDULength`
/// (262144). The P-DATA-TF length field is 32-bit, so the old 65535 ceiling
/// needlessly rejected compliant large PDUs (03-network-protocol §2.5).
pub const MAX_PDU_SIZE: u32 = 256 * 1024;

/// Clamps a configured max-PDU length to what this implementation can receive.
///
/// 0 means "unlimited" on the wire (PS3.8 §9.3.2), which this implementation
/// cannot honour, so it becomes [`MAX_PDU_SIZE`].
pub fn clamp_max_pdu_length(configured: u32) -> u32 {
    if configured == 0 {
        MAX_PDU_SIZE
    } else {
        configured.min(MAX_PDU_SIZE)
    }
}

/// The Application Context Name every DICOM association uses.
pub const APPLICATION_CONTEXT: &str = "1.2.840.10008.3.1.1.1";

/// PDU type codes (§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PduType {
    AssociateRq = 0x01,
    AssociateAc = 0x02,
    AssociateRj = 0x03,
    PDataTf = 0x04,
    ReleaseRq = 0x05,
    ReleaseRp = 0x06,
    Abort = 0x07,
}

impl PduType {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0x01 => PduType::AssociateRq,
            0x02 => PduType::AssociateAc,
            0x03 => PduType::AssociateRj,
            0x04 => PduType::PDataTf,
            0x05 => PduType::ReleaseRq,
            0x06 => PduType::ReleaseRp,
            0x07 => PduType::Abort,
            _ => return None,
        })
    }
}

/// A decoded PDU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pdu {
    AssociateRq(Associate),
    AssociateAc(Associate),
    AssociateRj(AssociateRj),
    PData(PData),
    ReleaseRq,
    ReleaseRp,
    Abort(Abort),
}

impl Pdu {
    pub fn pdu_type(&self) -> PduType {
        match self {
            Pdu::AssociateRq(_) => PduType::AssociateRq,
            Pdu::AssociateAc(_) => PduType::AssociateAc,
            Pdu::AssociateRj(_) => PduType::AssociateRj,
            Pdu::PData(_) => PduType::PDataTf,
            Pdu::ReleaseRq => PduType::ReleaseRq,
            Pdu::ReleaseRp => PduType::ReleaseRp,
            Pdu::Abort(_) => PduType::Abort,
        }
    }
}

/// Item type codes used inside A-ASSOCIATE and user information (§3.2).
mod item {
    pub const APPLICATION_CONTEXT: u8 = 0x10;
    pub const PRESENTATION_CONTEXT_RQ: u8 = 0x20;
    pub const PRESENTATION_CONTEXT_AC: u8 = 0x21;
    pub const ABSTRACT_SYNTAX: u8 = 0x30;
    pub const TRANSFER_SYNTAX: u8 = 0x40;
    pub const USER_INFORMATION: u8 = 0x50;
    pub const MAX_PDU_LENGTH: u8 = 0x51;
    pub const IMPLEMENTATION_CLASS_UID: u8 = 0x52;
    pub const ASYNC_OPERATIONS: u8 = 0x53;
    pub const ROLE_SELECTION: u8 = 0x54;
    pub const IMPLEMENTATION_VERSION_NAME: u8 = 0x55;
    pub const SOP_CLASS_EXTENDED_NEGOTIATION: u8 = 0x56;
    pub const SOP_CLASS_COMMON_EXTENDED: u8 = 0x57;
    pub const USER_IDENTITY_RQ: u8 = 0x58;
    pub const USER_IDENTITY_AC: u8 = 0x59;
}

/// One proposed or accepted presentation context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentationContext {
    /// Odd, 1..=255 (§4.1).
    pub id: u8,
    pub abstract_syntax: String,
    pub transfer_syntaxes: Vec<String>,
    /// AC only: 0 = acceptance, otherwise the reject reason.
    pub result: u8,
}

impl PresentationContext {
    pub fn new(id: u8, abstract_syntax: impl Into<String>, transfer_syntaxes: Vec<String>) -> Self {
        PresentationContext { id, abstract_syntax: abstract_syntax.into(), transfer_syntaxes, result: 0 }
    }

    pub fn is_accepted(&self) -> bool {
        self.result == 0
    }
}

/// Role selection pair for one abstract syntax (§4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSelection {
    pub sop_class: String,
    pub scu_role: bool,
    pub scp_role: bool,
}

/// Extended negotiation for one SOP class (§4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtendedNegotiation {
    pub sop_class: String,
    /// Service Class Application Information, raw across the wire: the byte at
    /// index `n - 1` is field `n`, one byte per field, no length prefixes
    /// (PS3.7 §D.3.3.5).
    pub requested: Vec<u8>,
    /// AC only.
    pub accepted: Vec<u8>,
}

/// Asynchronous operations window (§3.2, item 0x53).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsyncOperations {
    pub invoked: u16,
    pub performed: u16,
}

impl Default for AsyncOperations {
    fn default() -> Self {
        // (1,1) is the default and is not encoded on the wire
        AsyncOperations { invoked: 1, performed: 1 }
    }
}

impl AsyncOperations {
    /// The item is only present when the pair differs from (1,1).
    pub fn is_default(&self) -> bool {
        self.invoked == 1 && self.performed == 1
    }

    // The engine is deliberately serial (one association operation at a time),
    // so it always negotiates (1,1) and never advertises a window it would not
    // honour. The type exists to decode and round-trip a peer's item.
}

/// User identity negotiation (§4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserIdentity {
    /// 1 = username, 2 = username+passcode, 3 = Kerberos, 4 = SAML, 5 = JWT.
    pub identity_type: u8,
    pub primary: Vec<u8>,
    /// Only meaningful for type 2.
    pub secondary: Vec<u8>,
    pub positive_response_requested: bool,
}

impl UserIdentity {
    /// Validation rule: a secondary field is only allowed for type 2.
    pub fn validate(&self) -> Result<()> {
        if !(1..=5).contains(&self.identity_type) {
            return Err(NetworkError::protocol("user identity type must be 1..=5"));
        }
        if self.identity_type != 2 && !self.secondary.is_empty() {
            return Err(NetworkError::protocol("user identity passcode requires type 2"));
        }
        Ok(())
    }
}

/// User information sub-items (§3.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserInformation {
    pub max_pdu_length: u32,
    pub implementation_class_uid: Option<String>,
    pub implementation_version_name: Option<String>,
    pub async_operations: Option<AsyncOperations>,
    pub role_selections: Vec<RoleSelection>,
    pub extended_negotiations: Vec<ExtendedNegotiation>,
    /// Common extended negotiation (RQ only).
    pub common_extended: Option<CommonExtended>,
    pub user_identity: Option<UserIdentity>,
    /// AC only: the server's answer to a user identity request.
    pub user_identity_response: Option<Vec<u8>>,
}

/// SOP class common extended negotiation (§4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonExtended {
    pub service_class_uid: String,
    pub related_general_sop_classes: Vec<String>,
}

impl UserInformation {
    /// A minimal user information item with the given maximum PDU size.
    pub fn new(max_pdu_length: u32) -> Self {
        UserInformation { max_pdu_length, ..Default::default() }
    }
}

/// A-ASSOCIATE-RQ or A-ASSOCIATE-AC body (§3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Associate {
    pub protocol_version: u16,
    pub called_ae: String,
    pub calling_ae: String,
    pub application_context: String,
    pub presentation_contexts: Vec<PresentationContext>,
    pub user_information: UserInformation,
}

impl Associate {
    /// A fresh association with the standard application context.
    pub fn new(called_ae: impl Into<String>, calling_ae: impl Into<String>) -> Self {
        Associate {
            protocol_version: 1,
            called_ae: into_ae(called_ae),
            calling_ae: into_ae(calling_ae),
            application_context: APPLICATION_CONTEXT.to_string(),
            presentation_contexts: Vec::new(),
            user_information: UserInformation::new(16384),
        }
    }
}

/// A-ASSOCIATE-RJ body (§3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssociateRj {
    pub result: RejectResult,
    pub source: RejectSource,
    pub reason: u8,
}

/// One PDV inside a P-DATA-TF (§3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdv {
    pub context_id: u8,
    /// True for a command PDV, false for a dataset PDV.
    pub is_command: bool,
    /// True when this PDV completes the message unit.
    pub is_last: bool,
    /// The PDV value. `Bytes` so a decoded PDV shares its frame's buffer
    /// instead of copying it (§3.4).
    pub data: Bytes,
}

/// P-DATA-TF body (§3.3).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PData {
    pub pdvs: Vec<Pdv>,
}

/// A-ABORT body (§3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Abort {
    pub source: AbortSource,
    pub reason: u8,
}

// -- decoding ------------------------------------------------------------

/// Decodes one PDU from a complete frame (header included).
///
/// Borrowed convenience wrapper; the transport uses [`decode_bytes`] so a
/// P-DATA frame is not copied.
pub fn decode(bytes: &[u8]) -> Result<Pdu> {
    decode_bytes(Bytes::copy_from_slice(bytes))
}

/// Decodes one PDU from an owned frame, letting each P-DATA PDV share the
/// frame's buffer (`Bytes::slice`) instead of copying it (§3.4).
pub fn decode_bytes(frame: Bytes) -> Result<Pdu> {
    if frame.len() < 6 {
        return Err(NetworkError::protocol("PDU is shorter than its 6-byte header"));
    }
    let pdu_type = PduType::from_code(frame[0]).ok_or(NetworkError::protocol("unknown PDU type"))?;
    let length = u32::from_be_bytes([frame[2], frame[3], frame[4], frame[5]]);
    if length > MAX_PDU_SIZE {
        return Err(NetworkError::protocol("PDU length exceeds the 262144-byte receive limit"));
    }
    let end = 6 + length as usize;
    if frame.len() < end {
        return Err(NetworkError::protocol("PDU body is truncated"));
    }
    let body = frame.slice(6..end);

    Ok(match pdu_type {
        PduType::AssociateRq => Pdu::AssociateRq(decode_associate(&body, true)?),
        PduType::AssociateAc => Pdu::AssociateAc(decode_associate(&body, false)?),
        PduType::AssociateRj => Pdu::AssociateRj(decode_associate_rj(&body)?),
        PduType::PDataTf => Pdu::PData(decode_pdata_bytes(body)?),
        PduType::ReleaseRq => Pdu::ReleaseRq,
        PduType::ReleaseRp => Pdu::ReleaseRp,
        PduType::Abort => Pdu::Abort(decode_abort(&body)?),
    })
}

/// Reads a big-endian `u16` at `at`.
fn u16_at(bytes: &[u8], at: usize) -> Result<u16> {
    let slice = bytes.get(at..at + 2).ok_or(NetworkError::protocol("truncated u16"))?;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}

/// Reads a big-endian `u32` at `at`.
fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    let slice = bytes.get(at..at + 4).ok_or(NetworkError::protocol("truncated u32"))?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// AE titles are 16 bytes, space padded (§3.2).
fn decode_ae(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end_matches([' ', '\0']).to_string()
}

/// Pads or truncates an AE title to at most 16 **bytes** (PS3.8 §9.3.2).
///
/// The cut lands on a UTF-8 boundary so the value stays valid text instead of
/// being split mid-sequence; `encode_ae` then pads it to exactly 16 bytes.
fn into_ae(value: impl Into<String>) -> String {
    let value: String = value.into();
    let mut end = value.len().min(16);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn encode_ae(value: &str) -> [u8; 16] {
    let mut out = [b' '; 16];
    for (i, b) in value.as_bytes().iter().take(16).enumerate() {
        out[i] = *b;
    }
    out
}

fn decode_associate(body: &[u8], is_rq: bool) -> Result<Associate> {
    if body.len() < 68 {
        return Err(NetworkError::protocol("A-ASSOCIATE body is truncated"));
    }
    let protocol_version = u16_at(body, 0)?;
    let called_ae = decode_ae(&body[4..20]);
    let calling_ae = decode_ae(&body[20..36]);

    let mut offset = 68usize;
    let mut application_context = String::new();
    let mut presentation_contexts = Vec::new();
    let mut user_information = UserInformation::default();

    while offset + 4 <= body.len() {
        let (item_type, start, end) = item_header(body, offset)?;
        let value = &body[start..end];
        match item_type {
            item::APPLICATION_CONTEXT => {
                application_context = decode_string(value);
            }
            item::PRESENTATION_CONTEXT_RQ | item::PRESENTATION_CONTEXT_AC => {
                presentation_contexts.push(decode_presentation_context(value)?);
            }
            item::USER_INFORMATION => {
                user_information = decode_user_information(value, is_rq)?;
            }
            // unknown top-level items are skipped by length (§3.2)
            _ => {}
        }
        offset = end;
    }

    Ok(Associate {
        protocol_version,
        called_ae,
        calling_ae,
        application_context,
        presentation_contexts,
        user_information,
    })
}

fn decode_presentation_context(value: &[u8]) -> Result<PresentationContext> {
    if value.len() < 4 {
        return Err(NetworkError::protocol("presentation context is truncated"));
    }
    let id = value[0];
    // ID(1) Reserved(1) Result/Reason(1) Reserved(1) — PS3.8 §9.3.3.2
    let result = value[2];
    let mut abstract_syntax = String::new();
    let mut transfer_syntaxes = Vec::new();

    let mut offset = 4usize;
    while offset + 4 <= value.len() {
        let (item_type, start, end) = item_header(value, offset)?;
        let uid = decode_string(&value[start..end]);
        match item_type {
            item::ABSTRACT_SYNTAX => abstract_syntax = uid,
            item::TRANSFER_SYNTAX => transfer_syntaxes.push(uid),
            _ => {}
        }
        offset = end;
    }

    Ok(PresentationContext { id, abstract_syntax, transfer_syntaxes, result })
}

/// Decodes the user-information sub-items.
///
/// Item 0x56 means "requested" in an RQ and "accepted" in an AC: the wire
/// cannot say which, so the association direction decides (§4.3).
fn decode_user_information(value: &[u8], is_rq: bool) -> Result<UserInformation> {
    let mut out = UserInformation::default();
    let mut offset = 0usize;
    while offset + 4 <= value.len() {
        let (item_type, start, end) = item_header(value, offset)?;
        let raw = &value[start..end];
        match item_type {
            item::MAX_PDU_LENGTH => {
                if raw.len() >= 4 {
                    out.max_pdu_length = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
                }
            }
            item::IMPLEMENTATION_CLASS_UID => {
                out.implementation_class_uid = Some(decode_string(raw));
            }
            item::IMPLEMENTATION_VERSION_NAME => {
                out.implementation_version_name = Some(decode_string(raw));
            }
            item::ASYNC_OPERATIONS => {
                if raw.len() >= 4 {
                    out.async_operations = Some(AsyncOperations {
                        invoked: u16::from_be_bytes([raw[0], raw[1]]),
                        performed: u16::from_be_bytes([raw[2], raw[3]]),
                    });
                }
            }
            item::ROLE_SELECTION => {
                // [uid_len u16][uid][scu][scp] — PS3.8 §D.3.3.4
                if raw.len() >= 4 {
                    let uid_len = u16::from_be_bytes([raw[0], raw[1]]) as usize;
                    let uid = raw
                        .get(2..2 + uid_len)
                        .ok_or(NetworkError::protocol("role selection UID runs past the item"))?;
                    let scu = *raw
                        .get(2 + uid_len)
                        .ok_or(NetworkError::protocol("role selection has no SCU role"))?;
                    let scp = *raw
                        .get(3 + uid_len)
                        .ok_or(NetworkError::protocol("role selection has no SCP role"))?;
                    out.role_selections.push(RoleSelection {
                        sop_class: String::from_utf8_lossy(uid).to_string(),
                        scu_role: scu != 0,
                        scp_role: scp != 0,
                    });
                }
            }
            item::SOP_CLASS_EXTENDED_NEGOTIATION => {
                let mut ext = decode_extended_negotiation(raw)?;
                if !is_rq {
                    // the decoder fills `requested`; in an AC those bytes are the answer
                    ext.accepted = std::mem::take(&mut ext.requested);
                }
                out.extended_negotiations.push(ext);
            }
            item::SOP_CLASS_COMMON_EXTENDED => {
                // common extended negotiation is RQ-only (PS3.7)
                if is_rq {
                    out.common_extended = Some(decode_common_extended(raw)?);
                }
            }
            item::USER_IDENTITY_RQ => {
                // type(1) positive(1) primary_len(2) primary
                // secondary_len(2) secondary — PS3.8 §D.3.3.7
                if raw.len() >= 4 {
                    let primary_len = u16::from_be_bytes([raw[2], raw[3]]) as usize;
                    let primary = raw
                        .get(4..4 + primary_len)
                        .ok_or(NetworkError::protocol("user identity primary field runs past the item"))?
                        .to_vec();
                    let secondary_at = 4 + primary_len;
                    let secondary_len = u16_at(raw, secondary_at)? as usize;
                    let secondary = raw
                        .get(secondary_at + 2..secondary_at + 2 + secondary_len)
                        .ok_or(NetworkError::protocol("user identity secondary field runs past the item"))?
                        .to_vec();
                    out.user_identity = Some(UserIdentity {
                        identity_type: raw[0],
                        primary,
                        secondary,
                        positive_response_requested: raw[1] != 0,
                    });
                }
            }
            item::USER_IDENTITY_AC => {
                // PS3.8 §D.3.3.7: the item body is a length-prefixed server
                // response, `[server-response-length u16][response bytes]`.
                let server_len = u16_at(raw, 0)? as usize;
                let response = raw.get(2..2 + server_len).ok_or(NetworkError::protocol(
                    "user identity server response runs past the item",
                ))?;
                out.user_identity_response = Some(response.to_vec());
            }
            // unknown sub-items are skipped by length
            _ => {}
        }
        offset = end;
    }
    Ok(out)
}

fn decode_extended_negotiation(raw: &[u8]) -> Result<ExtendedNegotiation> {
    if raw.len() < 2 {
        return Err(NetworkError::protocol("extended negotiation is truncated"));
    }
    let uid_len = u16::from_be_bytes([raw[0], raw[1]]) as usize;
    let uid = raw
        .get(2..2 + uid_len)
        .ok_or(NetworkError::protocol("extended negotiation UID runs past the item"))?;
    Ok(ExtendedNegotiation {
        sop_class: String::from_utf8_lossy(uid).to_string(),
        // The rest is the flat Service Class Application Information array. An
        // odd-length array is even-padded by the item framing and that pad byte
        // is indistinguishable from a real field here, so it reads back as one
        // trailing 0 field — 0 means "not supported", so no grant is invented.
        requested: raw[2 + uid_len..].to_vec(),
        accepted: Vec::new(),
    })
}

fn decode_common_extended(raw: &[u8]) -> Result<CommonExtended> {
    if raw.len() < 2 {
        return Err(NetworkError::protocol("common extended negotiation is truncated"));
    }
    let uid_len = u16::from_be_bytes([raw[0], raw[1]]) as usize;
    let service_class_uid = String::from_utf8_lossy(
        raw.get(2..2 + uid_len)
            .ok_or(NetworkError::protocol("common extended service class UID runs past the item"))?,
    )
    .to_string();
    let mut offset = 2 + uid_len;
    let mut related = Vec::new();
    while offset + 2 <= raw.len() {
        let len = u16::from_be_bytes([raw[offset], raw[offset + 1]]) as usize;
        let start = offset + 2;
        // a length that overruns the item is malformed input, not a place to
        // stop reading: match `item_header` and fail loudly
        let uid = raw
            .get(start..start + len)
            .ok_or(NetworkError::protocol("common extended related SOP class runs past the item"))?;
        related.push(String::from_utf8_lossy(uid).to_string());
        offset = start + len;
    }
    Ok(CommonExtended { service_class_uid, related_general_sop_classes: related })
}

fn decode_associate_rj(body: &[u8]) -> Result<AssociateRj> {
    // reserved(1) result(1) source(1) reason(1)
    if body.len() < 4 {
        return Err(NetworkError::protocol("A-ASSOCIATE-RJ body is truncated"));
    }
    let result = RejectResult::from_code(body[1]).ok_or(NetworkError::protocol("unknown reject result"))?;
    let source = RejectSource::from_code(body[2]).ok_or(NetworkError::protocol("unknown reject source"))?;
    Ok(AssociateRj { result, source, reason: body[3] })
}

fn decode_pdata_bytes(body: Bytes) -> Result<PData> {
    // The PDV count is bounded by the PDU length, but preallocation is capped
    // so a large count cannot front-load a huge allocation; the vector grows as
    // PDVs actually arrive (§3.5).
    const PDV_COUNT_PREALLOC_CAP: usize = 1024;
    let mut pdvs = Vec::with_capacity((body.len() / 6).min(PDV_COUNT_PREALLOC_CAP));
    let mut offset = 0usize;
    while offset < body.len() {
        let len = u32_at(body.as_ref(), offset)? as usize;
        let start = offset + 4;
        let end = start.checked_add(len).ok_or(NetworkError::protocol("PDV length overflow"))?;
        if end > body.len() || len < 2 {
            return Err(NetworkError::protocol("PDV runs past the PDU"));
        }
        let context_id = body[start];
        let header = body[start + 1];
        // The PDV value is even-padded on the wire and the pad byte is
        // indistinguishable from real data at this layer, so it is kept.
        // The DIMSE assembler delimits real content by the command group
        // length and by dataset parsing, which tolerate trailing padding.
        // `slice` shares the frame buffer: no per-PDV copy (§3.4).
        let value = body.slice(start + 2..end);
        pdvs.push(Pdv {
            context_id,
            is_command: header & 0x01 != 0,
            is_last: header & 0x02 != 0,
            data: value,
        });
        offset = end;
    }
    Ok(PData { pdvs })
}

fn decode_abort(body: &[u8]) -> Result<Abort> {
    if body.len() < 4 {
        return Err(NetworkError::protocol("A-ABORT body is truncated"));
    }
    let source = AbortSource::from_code(body[2]).ok_or(NetworkError::protocol("unknown abort source"))?;
    Ok(Abort { source, reason: body[3] })
}

// -- encoding ------------------------------------------------------------

/// Encodes a PDU into a complete frame (header included).
///
/// Fails when an A-ASSOCIATE item value does not fit its 16-bit Item-length
/// (PS3.8 §9.3.2): writing a wrapped length would corrupt the PDU silently.
pub fn encode(pdu: &Pdu) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_into(pdu, &mut out)?;
    Ok(out)
}

/// Appends one encoded frame to `out`.
///
/// P-DATA is encoded straight into `out` (the hot send path allocates no
/// intermediate body buffer); the other, rare PDU kinds build their body first.
pub fn encode_into(pdu: &Pdu, out: &mut Vec<u8>) -> Result<()> {
    if let Pdu::PData(p) = pdu {
        encode_pdata_into(p, out);
        return Ok(());
    }
    let body = encode_body(pdu)?;
    out.reserve(body.len() + 6);
    out.push(pdu.pdu_type() as u8);
    out.push(0);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(())
}

fn encode_body(pdu: &Pdu) -> Result<Vec<u8>> {
    Ok(match pdu {
        Pdu::AssociateRq(a) => encode_associate(a, true)?,
        Pdu::AssociateAc(a) => encode_associate(a, false)?,
        Pdu::AssociateRj(rj) => vec![0, rj.result.code(), rj.source.code(), rj.reason],
        Pdu::PData(_) => unreachable!("P-DATA is encoded directly by encode_into"),
        Pdu::ReleaseRq | Pdu::ReleaseRp => vec![0, 0, 0, 0],
        Pdu::Abort(a) => vec![0, 0, a.source.code(), a.reason],
    })
}

/// `type(1) reserved(1) length(2) value`, the item framing used inside A-ASSOCIATE
/// (PS3.8 §9.3.2: items carry a 16-bit Item-length, unlike P-DATA PDVs whose
/// length is 32-bit). Every item value must have even length; an odd value is
/// padded with a single trailing 0x00 — strict peers (fo-dicom, DCMTK, ...)
/// reject an A-ASSOCIATE whose item header or value length is wrong.
fn push_item(out: &mut Vec<u8>, item_type: u8, value: &[u8]) -> Result<()> {
    let pad = value.len() & 1;
    let len = u16::try_from(value.len() + pad)
        .map_err(|_| NetworkError::protocol("A-ASSOCIATE item value exceeds the 16-bit item length"))?;
    out.push(item_type);
    out.push(0); // reserved
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value);
    if pad == 1 {
        out.push(0x00);
    }
    Ok(())
}

/// Reads a DICOM UL item header at `at`: type(1) reserved(1) length(2).
/// Returns `(item_type, value_start, value_end)`.
fn item_header(buf: &[u8], at: usize) -> Result<(u8, usize, usize)> {
    let hdr = buf.get(at..at + 4).ok_or(NetworkError::protocol("truncated item header"))?;
    let len = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
    let start = at + 4;
    let end = start.checked_add(len).ok_or(NetworkError::protocol("item length overflow"))?;
    if end > buf.len() {
        return Err(NetworkError::protocol("item runs past its parent"));
    }
    Ok((hdr[0], start, end))
}

/// Decodes a UID / string item value, trimming the even-length pad a peer may
/// have appended (a single trailing 0x00, or spaces some senders use).
fn decode_string(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).trim_end_matches(['\0', ' ']).to_string()
}

fn encode_associate(a: &Associate, is_rq: bool) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(&a.protocol_version.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&encode_ae(&a.called_ae));
    out.extend_from_slice(&encode_ae(&a.calling_ae));
    out.extend_from_slice(&[0u8; 32]);

    push_item(&mut out, item::APPLICATION_CONTEXT, a.application_context.as_bytes())?;

    for pc in &a.presentation_contexts {
        let mut body = Vec::new();
        // ID(1) Reserved(1) Result/Reason(1) Reserved(1) — PS3.8 §9.3.3.2
        body.push(pc.id);
        body.push(0);
        body.push(if is_rq { 0 } else { pc.result });
        body.push(0);
        if is_rq {
            push_item(&mut body, item::ABSTRACT_SYNTAX, pc.abstract_syntax.as_bytes())?;
            for syntax in &pc.transfer_syntaxes {
                push_item(&mut body, item::TRANSFER_SYNTAX, syntax.as_bytes())?;
            }
        } else if let Some(syntax) = pc.transfer_syntaxes.first() {
            // an A-ASSOCIATE-AC carries exactly one accepted transfer syntax and
            // never repeats the abstract syntax (§9.3.3)
            push_item(&mut body, item::TRANSFER_SYNTAX, syntax.as_bytes())?;
        }
        push_item(
            &mut out,
            if is_rq { item::PRESENTATION_CONTEXT_RQ } else { item::PRESENTATION_CONTEXT_AC },
            &body,
        )?;
    }

    let mut user = Vec::new();
    push_item(
        &mut user,
        item::MAX_PDU_LENGTH,
        &clamp_max_pdu_length(a.user_information.max_pdu_length).to_be_bytes(),
    )?;
    if let Some(uid) = &a.user_information.implementation_class_uid {
        push_item(&mut user, item::IMPLEMENTATION_CLASS_UID, uid.as_bytes())?;
    }
    if let Some(version) = &a.user_information.implementation_version_name {
        push_item(&mut user, item::IMPLEMENTATION_VERSION_NAME, version.as_bytes())?;
    }
    if let Some(async_ops) = a.user_information.async_operations {
        if !async_ops.is_default() {
            let mut value = Vec::with_capacity(4);
            value.extend_from_slice(&async_ops.invoked.to_be_bytes());
            value.extend_from_slice(&async_ops.performed.to_be_bytes());
            push_item(&mut user, item::ASYNC_OPERATIONS, &value)?;
        }
    }
    for role in &a.user_information.role_selections {
        // [uid_len u16][uid][scu][scp] — PS3.8 §D.3.3.4
        let mut value = Vec::with_capacity(4 + role.sop_class.len());
        value.extend_from_slice(&(role.sop_class.len() as u16).to_be_bytes());
        value.extend_from_slice(role.sop_class.as_bytes());
        value.push(u8::from(role.scu_role));
        value.push(u8::from(role.scp_role));
        push_item(&mut user, item::ROLE_SELECTION, &value)?;
    }
    for ext in &a.user_information.extended_negotiations {
        let info = if is_rq { &ext.requested } else { &ext.accepted };
        let mut value = Vec::with_capacity(2 + ext.sop_class.len() + info.len());
        value.extend_from_slice(&(ext.sop_class.len() as u16).to_be_bytes());
        value.extend_from_slice(ext.sop_class.as_bytes());
        // flat Service Class Application Information, one byte per field
        value.extend_from_slice(info);
        push_item(&mut user, item::SOP_CLASS_EXTENDED_NEGOTIATION, &value)?;
    }
    if let Some(common) = &a.user_information.common_extended {
        if is_rq {
            let mut value = Vec::new();
            value.extend_from_slice(&(common.service_class_uid.len() as u16).to_be_bytes());
            value.extend_from_slice(common.service_class_uid.as_bytes());
            for uid in &common.related_general_sop_classes {
                value.extend_from_slice(&(uid.len() as u16).to_be_bytes());
                value.extend_from_slice(uid.as_bytes());
            }
            push_item(&mut user, item::SOP_CLASS_COMMON_EXTENDED, &value)?;
        }
    }
    if let Some(identity) = &a.user_information.user_identity {
        if is_rq {
            // type(1) positive(1) primary_len(2) primary
            // secondary_len(2) secondary — PS3.8 §D.3.3.7
            let mut value = Vec::new();
            value.push(identity.identity_type);
            value.push(u8::from(identity.positive_response_requested));
            value.extend_from_slice(&(identity.primary.len() as u16).to_be_bytes());
            value.extend_from_slice(&identity.primary);
            value.extend_from_slice(&(identity.secondary.len() as u16).to_be_bytes());
            value.extend_from_slice(&identity.secondary);
            push_item(&mut user, item::USER_IDENTITY_RQ, &value)?;
        }
    }
    if let Some(response) = &a.user_information.user_identity_response {
        if !is_rq {
            // PS3.8 §D.3.3.7: server-response-length (u16) then the response
            if response.len() > u16::MAX as usize {
                return Err(NetworkError::protocol("user identity server response exceeds 65535 bytes"));
            }
            let mut value = Vec::with_capacity(2 + response.len());
            value.extend_from_slice(&(response.len() as u16).to_be_bytes());
            value.extend_from_slice(response);
            push_item(&mut user, item::USER_IDENTITY_AC, &value)?;
        }
    }
    push_item(&mut out, item::USER_INFORMATION, &user)?;
    Ok(out)
}

fn encode_pdata_into(p: &PData, out: &mut Vec<u8>) {
    // header placeholder: type(1) reserved(1) length(4), patched below
    let start = out.len();
    out.extend_from_slice(&[0x04, 0, 0, 0, 0, 0]);
    for pdv in &p.pdvs {
        // value is padded to an even length on the wire (§3.3)
        let pad = pdv.data.len() % 2;
        let value_len = pdv.data.len() + pad;
        out.extend_from_slice(&((value_len + 2) as u32).to_be_bytes());
        out.push(pdv.context_id);
        let mut header = 0u8;
        if pdv.is_command {
            header |= 0x01;
        }
        if pdv.is_last {
            header |= 0x02;
        }
        out.push(header);
        out.extend_from_slice(&pdv.data);
        if pad == 1 {
            out.push(0);
        }
    }
    let body_len = (out.len() - start - 6) as u32;
    out[start + 2..start + 6].copy_from_slice(&body_len.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoder, for the tests that expect a well-formed PDU.
    fn encode(pdu: &Pdu) -> Vec<u8> {
        super::encode(pdu).expect("encode")
    }

    fn round_trip(pdu: Pdu) -> Pdu {
        let bytes = encode(&pdu);
        assert_eq!(bytes.len(), 6 + u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]) as usize);
        decode(&bytes).expect("decode")
    }

    #[test]
    fn encode_into_appends_the_same_bytes_as_encode() {
        let pdu = Pdu::PData(PData {
            pdvs: vec![Pdv { context_id: 3, is_command: true, is_last: true, data: vec![1, 2, 3].into() }],
        });
        let frame = encode(&pdu);
        let mut buffer = vec![0xAA];
        super::encode_into(&pdu, &mut buffer).expect("encode");
        assert_eq!(&buffer[1..], &frame[..], "encode_into matches encode byte for byte");
        super::encode_into(&pdu, &mut buffer).expect("encode");
        assert_eq!(&buffer[1 + frame.len()..], &frame[..], "a second frame appends cleanly");
    }

    /// Storage SOP class used by the byte-layout fixtures below.
    const STORAGE_CLASS: &str = "1.2.840.10008.5.1.4.1.1.7";

    /// `type(1) reserved(1) length(2) value`, padded to an even value length.
    ///
    /// Hand-built on purpose: the wire-layout tests must not reuse the encoder
    /// under test, or a shared wrong assumption would stay invisible.
    fn item(item_type: u8, value: &[u8]) -> Vec<u8> {
        let pad = value.len() & 1;
        let mut out = vec![item_type, 0];
        out.extend_from_slice(&((value.len() + pad) as u16).to_be_bytes());
        out.extend_from_slice(value);
        out.resize(out.len() + pad, 0);
        out
    }

    fn ae(title: &str) -> Vec<u8> {
        let mut out = vec![b' '; 16];
        out[..title.len()].copy_from_slice(title.as_bytes());
        out
    }

    /// A complete A-ASSOCIATE frame from PS3.8 field layouts only.
    fn associate_frame(pdu_type: u8, items: &[Vec<u8>]) -> Vec<u8> {
        let mut body = vec![0x00, 0x01, 0x00, 0x00]; // protocol version, reserved
        body.extend_from_slice(&ae("CALLED"));
        body.extend_from_slice(&ae("CALLING"));
        body.extend_from_slice(&[0u8; 32]);
        for item in items {
            body.extend_from_slice(item);
        }
        let mut frame = vec![pdu_type, 0];
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&body);
        frame
    }

    /// The value of the first item of `item_type` in `buf`.
    fn item_value(buf: &[u8], item_type: u8) -> Option<Vec<u8>> {
        let mut offset = 0usize;
        while offset + 4 <= buf.len() {
            let len = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
            let start = offset + 4;
            let end = start + len;
            if end > buf.len() {
                return None;
            }
            if buf[offset] == item_type {
                return Some(buf[start..end].to_vec());
            }
            offset = end;
        }
        None
    }

    /// The user-information item value of an encoded A-ASSOCIATE.
    fn user_items(bytes: &[u8]) -> Vec<u8> {
        item_value(&bytes[6 + 68..], item::USER_INFORMATION).expect("user information item")
    }

    #[test]
    fn release_pdus_round_trip() {
        assert_eq!(round_trip(Pdu::ReleaseRq), Pdu::ReleaseRq);
        assert_eq!(round_trip(Pdu::ReleaseRp), Pdu::ReleaseRp);
        let bytes = encode(&Pdu::ReleaseRq);
        assert_eq!(bytes, vec![0x05, 0x00, 0x00, 0x00, 0x00, 0x04, 0, 0, 0, 0]);
    }

    #[test]
    fn abort_round_trips() {
        let abort = Abort { source: AbortSource::Provider, reason: 2 };
        assert_eq!(round_trip(Pdu::Abort(abort.clone())), Pdu::Abort(abort));
        let bytes = encode(&Pdu::Abort(Abort { source: AbortSource::User, reason: 0 }));
        assert_eq!(bytes, vec![0x07, 0x00, 0x00, 0x00, 0x00, 0x04, 0, 0, 0, 0]);
    }

    #[test]
    fn reject_round_trips() {
        let rj = AssociateRj {
            result: RejectResult::Permanent,
            source: RejectSource::ProviderAcse,
            reason: 7,
        };
        assert_eq!(round_trip(Pdu::AssociateRj(rj.clone())), Pdu::AssociateRj(rj));
    }

    #[test]
    fn associate_rq_round_trips() {
        let mut assoc = Associate::new("STORE-SCP", "MY-SCU");
        assoc.presentation_contexts.push(PresentationContext::new(
            1,
            "1.2.840.10008.1.1",
            vec!["1.2.840.10008.1.2".to_string(), "1.2.840.10008.1.2.1".to_string()],
        ));
        assoc.user_information.implementation_class_uid = Some("1.2.3.4".into());
        assoc.user_information.implementation_version_name = Some("RADICOM_0_1".into());
        assoc.user_information.max_pdu_length = 32768;

        match round_trip(Pdu::AssociateRq(assoc.clone())) {
            Pdu::AssociateRq(back) => {
                assert_eq!(back.called_ae, "STORE-SCP");
                assert_eq!(back.calling_ae, "MY-SCU");
                assert_eq!(back.protocol_version, 1);
                assert_eq!(back.application_context, APPLICATION_CONTEXT);
                assert_eq!(back.presentation_contexts, assoc.presentation_contexts);
                assert_eq!(back.user_information.max_pdu_length, 32768);
                assert_eq!(back.user_information.implementation_class_uid.as_deref(), Some("1.2.3.4"));
            }
            other => panic!("expected AssociateRq, got {other:?}"),
        }
    }

    #[test]
    fn associate_ac_carries_results_and_one_syntax() {
        let mut assoc = Associate::new("STORE-SCP", "MY-SCU");
        let mut accepted = PresentationContext::new(1, "1.2.840.10008.1.1", vec!["1.2.840.10008.1.2".to_string()]);
        accepted.result = 0;
        let mut rejected = PresentationContext::new(3, "1.2.840.10008.5.1.4.1.1.7", vec!["1.2.840.10008.1.2".to_string()]);
        rejected.result = 3; // abstract syntax not supported
        assoc.presentation_contexts = vec![accepted, rejected];

        match round_trip(Pdu::AssociateAc(assoc)) {
            Pdu::AssociateAc(back) => {
                assert!(back.presentation_contexts[0].is_accepted());
                assert_eq!(back.presentation_contexts[0].transfer_syntaxes.len(), 1, "exactly one syntax on accept");
                assert_eq!(back.presentation_contexts[1].result, 3);
                assert!(!back.presentation_contexts[1].is_accepted());
            }
            other => panic!("expected AssociateAc, got {other:?}"),
        }
    }

    #[test]
    fn ae_titles_are_padded_and_trimmed() {
        let assoc = Associate::new("SHORT", "A-VERY-LONG-AE-TITLE");
        let bytes = encode(&Pdu::AssociateRq(assoc));
        // called AE occupies bytes 6+4 .. 6+20 and must be space padded
        assert_eq!(&bytes[10..26], b"SHORT           ");
        // calling AE is truncated to 16 *bytes*
        assert_eq!(&bytes[26..42], b"A-VERY-LONG-AE-T");
        match decode(&bytes).unwrap() {
            Pdu::AssociateRq(back) => {
                assert_eq!(back.called_ae, "SHORT", "trailing spaces trimmed");
                assert_eq!(back.calling_ae, "A-VERY-LONG-AE-T");
            }
            other => panic!("expected AssociateRq, got {other:?}"),
        }
    }

    #[test]
    fn ae_titles_are_truncated_by_bytes_not_characters() {
        // PS3.8 §9.3.2: an AE title is 16 bytes. A multi-byte title must be cut
        // to at most 16 bytes on a character boundary, never to 16 characters
        // (which could be 48 bytes and would be split mid-sequence on the wire).
        let wide = "日本語日本語日本語"; // 9 chars, 27 bytes
        assert_eq!(wide.len(), 27);
        let assoc = Associate::new(wide, "SCU");
        assert!(assoc.called_ae.len() <= 16, "got {} bytes", assoc.called_ae.len());
        assert!(wide.starts_with(&assoc.called_ae), "the cut is a prefix, not a re-encoding");

        let bytes = encode(&Pdu::AssociateRq(assoc));
        match decode(&bytes).unwrap() {
            Pdu::AssociateRq(back) => {
                assert!(!back.called_ae.contains('\u{FFFD}'), "no broken UTF-8 on the wire: {:?}", back.called_ae);
                assert_eq!(back.called_ae.len(), 15, "five 3-byte characters fit in 16 bytes");
            }
            other => panic!("expected AssociateRq, got {other:?}"),
        }
    }

    #[test]
    fn an_oversized_associate_item_is_a_protocol_error() {
        // An item value that does not fit the 16-bit Item-length must be
        // refused, not written with a wrapped length (PS3.8 §9.3.2).
        let mut assoc = Associate::new("SCP", "SCU");
        let uid = "1.2.840.10008.5.1.4.1.1.7.1.1.1.1.1.1.1.1.1.1.1.1";
        for _ in 0..2000 {
            assoc.user_information.role_selections.push(RoleSelection {
                sop_class: uid.to_string(),
                scu_role: true,
                scp_role: false,
            });
        }
        match super::encode(&Pdu::AssociateRq(assoc)) {
            Err(NetworkError::Protocol(message)) => assert!(message.contains("16-bit item length"), "got {message}"),
            other => panic!("expected a Protocol error, got {:?}", other.map(|b| b.len())),
        }
    }

    #[test]
    fn associate_item_values_are_even_length_on_the_wire() {
        // Odd-length UIDs (application context 21 bytes, Verification abstract
        // syntax 17, Explicit-VR-LE transfer syntax 19) must be padded to even
        // length everywhere (PS3.8 §9.3). Strict peers (fo-dicom/DCMTK) reject
        // an A-ASSOCIATE whose item values are odd.
        fn assert_items_even(buf: &[u8], depth: usize) {
            let mut offset = 0usize;
            while offset + 4 <= buf.len() {
                let item_type = buf[offset];
                let len = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
                assert_eq!(len % 2, 0, "item 0x{item_type:02X} at depth {depth} has odd value length {len}");
                let val = &buf[offset + 4..offset + 4 + len];
                match item_type {
                    item::PRESENTATION_CONTEXT_RQ | item::PRESENTATION_CONTEXT_AC => assert_items_even(&val[4..], depth + 1),
                    item::USER_INFORMATION => assert_items_even(val, depth + 1),
                    _ => {}
                }
                offset += 4 + len;
            }
            // any remaining bytes must be even-length padding zeros
            assert!(buf[offset..].iter().all(|&b| b == 0), "non-pad trailing bytes at depth {depth}");
        }

        let mut assoc = Associate::new("NETGATE", "NETPUSH"); // both 7 chars (odd)
        assoc.presentation_contexts.push(PresentationContext::new(
            1,
            "1.2.840.10008.1.1",
            vec!["1.2.840.10008.1.2.1".to_string()],
        ));
        let bytes = encode(&Pdu::AssociateRq(assoc));
        assert_items_even(&bytes[6 + 68..], 0); // skip the 6-byte PDU header + 68-byte fixed part

        // decode round-trips the padded UIDs without a trailing NUL
        match decode(&bytes).unwrap() {
            Pdu::AssociateRq(back) => {
                assert_eq!(back.application_context, APPLICATION_CONTEXT);
                assert_eq!(back.presentation_contexts[0].abstract_syntax, "1.2.840.10008.1.1");
                assert_eq!(back.presentation_contexts[0].transfer_syntaxes[0], "1.2.840.10008.1.2.1");
            }
            other => panic!("expected AssociateRq, got {other:?}"),
        }
    }

    #[test]
    fn associate_ac_omits_abstract_syntax_and_carries_one_syntax() {
        let mut ac = Associate::new("SCU", "SCP");
        let mut pc = PresentationContext::new(
            1,
            "1.2.840.10008.5.1.4.1.1.7",
            vec!["1.2.840.10008.1.2".into(), "1.2.840.10008.1.2.1".into()],
        );
        pc.result = 0;
        ac.presentation_contexts.push(pc);
        let bytes = encode(&Pdu::AssociateAc(ac));
        // §9.3.3: an AC context answers with the result and exactly one accepted
        // transfer syntax; it never repeats the abstract syntax.
        match decode(&bytes).unwrap() {
            Pdu::AssociateAc(back) => {
                let p = &back.presentation_contexts[0];
                assert_eq!(p.id, 1);
                assert_eq!(p.result, 0);
                assert!(p.abstract_syntax.is_empty(), "AC omits the abstract syntax");
                assert_eq!(p.transfer_syntaxes, vec!["1.2.840.10008.1.2".to_string()]);
            }
            other => panic!("expected AssociateAc, got {other:?}"),
        }
    }

    #[test]
    fn associate_rj_body_is_reserved_result_source_reason() {
        let rj = AssociateRj {
            result: crate::error::RejectResult::Permanent,
            source: crate::error::RejectSource::User,
            reason: 3,
        };
        let bytes = encode(&Pdu::AssociateRj(rj.clone()));
        // header(6) + reserved(00) result(01) source(01) reason(03)
        assert_eq!(&bytes[6..10], &[0x00, 0x01, 0x01, 0x03]);
        assert!(matches!(decode(&bytes).unwrap(), Pdu::AssociateRj(back) if back == rj));
    }

    #[test]
    fn user_information_sub_items_round_trip() {
        let mut assoc = Associate::new("SCP", "SCU");
        assoc.user_information.async_operations = Some(AsyncOperations { invoked: 4, performed: 2 });
        assoc.user_information.role_selections.push(RoleSelection {
            sop_class: "1.2.840.10008.5.1.4.1.1.7".into(),
            scu_role: true,
            scp_role: false,
        });
        assoc.user_information.extended_negotiations.push(ExtendedNegotiation {
            sop_class: "1.2.840.10008.5.1.4.1.2.1.1".into(),
            requested: vec![1, 0, 1],
            accepted: Vec::new(),
        });
        assoc.user_information.common_extended = Some(CommonExtended {
            service_class_uid: "1.2.840.10008.4.2".into(),
            related_general_sop_classes: vec!["1.2.840.10008.5.1.4.1.1.7".into()],
        });
        assoc.user_information.user_identity = Some(UserIdentity {
            identity_type: 2,
            primary: b"alice".to_vec(),
            secondary: b"secret".to_vec(),
            positive_response_requested: true,
        });

        match round_trip(Pdu::AssociateRq(assoc)) {
            Pdu::AssociateRq(back) => {
                let ui = back.user_information;
                assert_eq!(ui.async_operations, Some(AsyncOperations { invoked: 4, performed: 2 }));
                assert!(ui.role_selections[0].scu_role);
                assert!(!ui.role_selections[0].scp_role);
                assert_eq!(ui.extended_negotiations[0].requested, vec![1, 0, 1]);
                assert_eq!(ui.common_extended.unwrap().service_class_uid, "1.2.840.10008.4.2");
                let identity = ui.user_identity.unwrap();
                assert_eq!(identity.identity_type, 2);
                assert_eq!(identity.primary, b"alice");
                assert_eq!(identity.secondary, b"secret");
                assert!(identity.positive_response_requested);
            }
            other => panic!("expected AssociateRq, got {other:?}"),
        }
    }

    #[test]
    fn extended_negotiation_lands_on_the_side_matching_the_pdu() {
        // item 0x56 carries "requested" in an RQ and "accepted" in an AC; the
        // wire cannot say which, so the PDU type must decide (§4.3)
        let mut assoc = Associate::new("SCP", "SCU");
        assoc.user_information.extended_negotiations.push(ExtendedNegotiation {
            sop_class: "1.2.840.10008.5.1.4.1.1.7".into(),
            requested: vec![1],
            accepted: Vec::new(),
        });
        match round_trip(Pdu::AssociateRq(assoc.clone())) {
            Pdu::AssociateRq(back) => {
                let ext = &back.user_information.extended_negotiations[0];
                assert_eq!(ext.requested, vec![1], "an RQ keeps the request");
                assert!(ext.accepted.is_empty());
            }
            other => panic!("expected AssociateRq, got {other:?}"),
        }

        assoc.user_information.extended_negotiations[0].requested = Vec::new();
        assoc.user_information.extended_negotiations[0].accepted = vec![1, 0, 1];
        match round_trip(Pdu::AssociateAc(assoc)) {
            Pdu::AssociateAc(back) => {
                let ext = &back.user_information.extended_negotiations[0];
                assert_eq!(ext.accepted, vec![1, 0, 1], "an AC keeps the answer");
                assert!(ext.requested.is_empty());
            }
            other => panic!("expected AssociateAc, got {other:?}"),
        }
    }

    #[test]
    fn default_async_operations_are_not_encoded() {
        let mut assoc = Associate::new("SCP", "SCU");
        assoc.user_information.async_operations = Some(AsyncOperations::default());
        let bytes = encode(&Pdu::AssociateRq(assoc));
        // item 0x53 must be absent: scan for the byte pattern after a 0x53
        assert!(!bytes.windows(1).enumerate().any(|(i, w)| w[0] == 0x53 && bytes.get(i + 1..i + 5) == Some(&[0, 0, 0, 4])),
            "(1,1) is the default and is omitted");
    }

    #[test]
    fn pdata_round_trips_and_pads_values() {
        let pdata = PData {
            pdvs: vec![
                Pdv { context_id: 1, is_command: true, is_last: true, data: vec![1, 2, 3, 4].into() },
                Pdv { context_id: 1, is_command: false, is_last: false, data: vec![9, 8, 7].into() },
            ],
        };
        match round_trip(Pdu::PData(pdata)) {
            Pdu::PData(back) => {
                assert_eq!(back.pdvs.len(), 2);
                assert!(back.pdvs[0].is_command && back.pdvs[0].is_last);
                assert_eq!(&back.pdvs[0].data[..], &[1, 2, 3, 4]);
                assert!(!back.pdvs[1].is_command && !back.pdvs[1].is_last);
                assert_eq!(&back.pdvs[1].data[..], &[9, 8, 7, 0], "pad byte is kept at the PDU layer");
            }
            other => panic!("expected PData, got {other:?}"),
        }
    }

    #[test]
    fn decode_bytes_matches_decode_and_preserves_pdvs() {
        // §3.4: the zero-copy owned-frame path must decode exactly like the
        // borrowed one (same PDVs, same bytes).
        let pdata = PData {
            pdvs: vec![
                Pdv { context_id: 1, is_command: true, is_last: true, data: vec![1, 2, 3, 4].into() },
                Pdv { context_id: 3, is_command: false, is_last: false, data: vec![9, 8, 7].into() },
            ],
        };
        let frame = encode(&Pdu::PData(pdata.clone()));
        let owned = super::decode_bytes(bytes::Bytes::from(frame.clone())).expect("decode_bytes");
        let borrowed = super::decode(&frame).expect("decode");
        assert_eq!(owned, borrowed, "both decode paths yield the same PDU");
        let Pdu::PData(back) = owned else { panic!("expected PData") };
        assert_eq!(&back.pdvs[0].data[..], &[1, 2, 3, 4]);
        assert_eq!(&back.pdvs[1].data[..], &[9, 8, 7, 0], "pad byte kept, as on the wire");
    }

    #[test]
    fn pdata_odd_value_is_even_on_the_wire() {
        let pdata = PData { pdvs: vec![Pdv { context_id: 1, is_command: true, is_last: true, data: vec![0xAA].into() }] };
        let bytes = encode(&Pdu::PData(pdata));
        // pdv-length(4) + id(1) + header(1) + value(2)
        assert_eq!(u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]), 4);
        assert_eq!(bytes[12], 0xAA);
        assert_eq!(bytes[13], 0x00, "pad byte appended");
    }

    #[test]
    fn unknown_items_are_skipped_by_length() {
        // hand-build an A-ASSOCIATE-RQ with an unknown top-level item 0x99
        let mut assoc = Associate::new("SCP", "SCU");
        assoc.presentation_contexts.push(PresentationContext::new(1, "1.2.840.10008.1.1", vec!["1.2.840.10008.1.2".into()]));
        let mut bytes = encode(&Pdu::AssociateRq(assoc));
        // locate the user-information item (type 0x50) by walking top-level
        // items — each header is type(1) reserved(1) length(2) — from the end of
        // the 6-byte PDU header + 68-byte fixed part.
        let mut insert_at = None;
        let mut offset = 6 + 68;
        while offset + 4 <= bytes.len() {
            let t = bytes[offset];
            let len = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
            if t == 0x50 {
                insert_at = Some(offset);
                break;
            }
            offset += 4 + len;
        }
        let insert_at = insert_at.expect("user information item present");
        // unknown item: type 0x99, reserved 0, 16-bit length of an even value
        let mut unknown = vec![0x99, 0x00];
        unknown.extend_from_slice(&6u16.to_be_bytes());
        unknown.extend_from_slice(b"junk!!");
        bytes.splice(insert_at..insert_at, unknown);
        // fix the PDU length
        let new_len = (bytes.len() - 6) as u32;
        bytes[2..6].copy_from_slice(&new_len.to_be_bytes());

        match decode(&bytes).expect("unknown item skipped") {
            Pdu::AssociateRq(back) => {
                assert_eq!(back.presentation_contexts.len(), 1, "known items still decoded");
                assert_eq!(back.user_information.max_pdu_length, 16384);
            }
            other => panic!("expected AssociateRq, got {other:?}"),
        }
    }

    #[test]
    fn malformed_input_never_panics() {
        // every truncation of a valid association must fail or succeed cleanly
        let mut assoc = Associate::new("SCP", "SCU");
        assoc.presentation_contexts.push(PresentationContext::new(1, "1.2.840.10008.1.1", vec!["1.2.840.10008.1.2".into()]));
        let bytes = encode(&Pdu::AssociateRq(assoc));
        for cut in 0..bytes.len() {
            let _ = decode(&bytes[..cut]);
        }
        // unknown PDU type
        assert!(matches!(decode(&[0x99, 0, 0, 0, 0, 0]), Err(NetworkError::Protocol(_))));
        // header shorter than 6 bytes
        assert!(matches!(decode(&[0x01, 0x00]), Err(NetworkError::Protocol(_))));
        // declared length beyond the buffer
        assert!(matches!(decode(&[0x05, 0, 0, 0, 0, 8, 0, 0]), Err(NetworkError::Protocol(_))));
        // length above the 262144 limit
        assert!(matches!(decode(&[0x05, 0, 0x00, 0x04, 0x00, 0x01]), Err(NetworkError::Protocol(_))));
    }

    #[test]
    fn random_bytes_do_not_panic() {
        // a small deterministic sweep over the first byte, which selects the
        // decoder, plus adversarial lengths
        for first in 0u8..=255 {
            for len in [0u8, 1, 2, 4, 8, 255] {
                let mut bytes = vec![first, 0, 0, 0, 0, len];
                bytes.extend(std::iter::repeat_n(0xAB, len as usize));
                let _ = decode(&bytes);
            }
        }
    }

    #[test]
    fn user_identity_validation_rules() {
        let mut identity = UserIdentity {
            identity_type: 1,
            primary: b"alice".to_vec(),
            secondary: b"secret".to_vec(),
            positive_response_requested: false,
        };
        assert!(identity.validate().is_err(), "type 1 must not carry a passcode");
        identity.secondary.clear();
        assert!(identity.validate().is_ok());

        identity.identity_type = 6;
        assert!(identity.validate().is_err(), "type must be 1..=5");
    }

    #[test]
    fn max_pdu_length_is_big_endian() {
        let mut assoc = Associate::new("SCP", "SCU");
        assoc.user_information.max_pdu_length = 0x1234;
        let bytes = encode(&Pdu::AssociateRq(assoc));
        let value = item_value(&user_items(&bytes), item::MAX_PDU_LENGTH).expect("max PDU item");
        assert_eq!(value, vec![0x00, 0x00, 0x12, 0x34]);
    }

    #[test]
    fn advertised_max_pdu_is_clamped_to_what_we_can_receive() {
        // A peer may send a P-DATA-TF up to the value we advertise, so a
        // larger configured value must never reach the wire: it would be
        // self-reported capacity this side then rejects on read.
        for configured in [MAX_PDU_SIZE + 1, u32::MAX, 0] {
            let mut assoc = Associate::new("SCP", "SCU");
            assoc.user_information.max_pdu_length = configured;
            let bytes = encode(&Pdu::AssociateRq(assoc));
            let value = item_value(&user_items(&bytes), item::MAX_PDU_LENGTH).expect("max PDU item");
            let advertised = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
            assert_eq!(advertised, MAX_PDU_SIZE, "configured {configured} is clamped");
        }
        // A value at or below the ceiling is advertised verbatim.
        let mut assoc = Associate::new("SCP", "SCU");
        assoc.user_information.max_pdu_length = 131072;
        let bytes = encode(&Pdu::AssociateRq(assoc));
        let value = item_value(&user_items(&bytes), item::MAX_PDU_LENGTH).expect("max PDU item");
        assert_eq!(u32::from_be_bytes([value[0], value[1], value[2], value[3]]), 131072);
    }

    #[test]
    fn associate_ac_result_lives_at_offset_two() {
        // PS3.8 §9.3.3.2: ID(1) Reserved(1) Result/Reason(1) Reserved(1).
        // Context 1 is accepted, context 3 is rejected with reason 3.
        let pc = |id: u8, result: u8| {
            let mut value = vec![id, 0x00, result, 0x00];
            value.extend_from_slice(&item(item::TRANSFER_SYNTAX, b"1.2.840.10008.1.2.1"));
            item(item::PRESENTATION_CONTEXT_AC, &value)
        };
        let frame = associate_frame(
            0x02,
            &[
                item(item::APPLICATION_CONTEXT, APPLICATION_CONTEXT.as_bytes()),
                pc(1, 0),
                pc(3, 3),
                item(item::USER_INFORMATION, &item(item::MAX_PDU_LENGTH, &131072u32.to_be_bytes())),
            ],
        );
        assert_eq!(frame.len(), 176, "the standard 176-byte A-ASSOCIATE-AC fixture");

        match decode(&frame).expect("decode") {
            Pdu::AssociateAc(ac) => {
                assert_eq!(ac.presentation_contexts.len(), 2);
                assert_eq!(ac.presentation_contexts[0].result, 0);
                assert!(ac.presentation_contexts[0].is_accepted());
                assert_eq!(ac.presentation_contexts[1].id, 3);
                assert_eq!(ac.presentation_contexts[1].result, 3, "the result is at offset 2");
                assert!(!ac.presentation_contexts[1].is_accepted());
                assert_eq!(ac.user_information.max_pdu_length, 131072, "the peer's value decodes as-is");
            }
            other => panic!("expected AssociateAc, got {other:?}"),
        }
    }

    #[test]
    fn presentation_context_encoder_writes_id_reserved_result_reserved() {
        let mut ac = Associate::new("SCP", "SCU");
        let mut rejected = PresentationContext::new(3, STORAGE_CLASS, vec!["1.2.840.10008.1.2.1".into()]);
        rejected.result = 3;
        ac.presentation_contexts = vec![rejected];
        let bytes = encode(&Pdu::AssociateAc(ac));
        let body = item_value(&bytes[6 + 68..], item::PRESENTATION_CONTEXT_AC).expect("presentation context");
        assert_eq!(&body[..4], &[0x03, 0x00, 0x03, 0x00], "ID, Reserved, Result, Reserved");

        // the RQ direction is the same layout with a zero result
        let mut rq = Associate::new("SCP", "SCU");
        rq.presentation_contexts = vec![PresentationContext::new(1, STORAGE_CLASS, vec!["1.2.840.10008.1.2.1".into()])];
        let bytes = encode(&Pdu::AssociateRq(rq));
        let body = item_value(&bytes[6 + 68..], item::PRESENTATION_CONTEXT_RQ).expect("presentation context");
        assert_eq!(&body[..4], &[0x01, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn role_selection_is_uid_length_then_uid_then_roles() {
        // PS3.8 §D.3.3.4: uid_len(2) + UID + SCU(1) + SCP(1)
        let mut value = Vec::new();
        value.extend_from_slice(&(STORAGE_CLASS.len() as u16).to_be_bytes());
        value.extend_from_slice(STORAGE_CLASS.as_bytes());
        value.push(1); // SCU role
        value.push(0); // SCP role
        let frame = associate_frame(
            0x01,
            &[
                item(item::APPLICATION_CONTEXT, APPLICATION_CONTEXT.as_bytes()),
                item(
                    item::USER_INFORMATION,
                    &[
                        item(item::MAX_PDU_LENGTH, &16384u32.to_be_bytes()),
                        item(item::ROLE_SELECTION, &value),
                    ]
                    .concat(),
                ),
            ],
        );
        match decode(&frame).expect("decode") {
            Pdu::AssociateRq(rq) => {
                let role = &rq.user_information.role_selections[0];
                assert_eq!(role.sop_class, STORAGE_CLASS, "the UID is not shifted");
                assert!(role.scu_role);
                assert!(!role.scp_role, "the SCP role is the last byte");
            }
            other => panic!("expected AssociateRq, got {other:?}"),
        }

        // encode direction: the same field order
        let mut rq = Associate::new("SCP", "SCU");
        rq.user_information.role_selections.push(RoleSelection {
            sop_class: STORAGE_CLASS.into(),
            scu_role: true,
            scp_role: false,
        });
        let bytes = encode(&Pdu::AssociateRq(rq));
        let value = item_value(&user_items(&bytes), item::ROLE_SELECTION).expect("role selection");
        assert_eq!(u16::from_be_bytes([value[0], value[1]]) as usize, STORAGE_CLASS.len(), "the UID length leads");
        assert_eq!(&value[2..2 + STORAGE_CLASS.len()], STORAGE_CLASS.as_bytes());
        assert_eq!(&value[2 + STORAGE_CLASS.len()..4 + STORAGE_CLASS.len()], &[1, 0], "SCU then SCP");
    }

    #[test]
    fn extended_negotiation_is_a_flat_field_array() {
        // PS3.7 §D.3.3.5: uid_len(2) + UID + one byte per field, no lengths
        let mut value = Vec::new();
        value.extend_from_slice(&(STORAGE_CLASS.len() as u16).to_be_bytes());
        value.extend_from_slice(STORAGE_CLASS.as_bytes());
        value.extend_from_slice(&[0, 1, 4]);
        let frame = associate_frame(
            0x01,
            &[
                item(item::APPLICATION_CONTEXT, APPLICATION_CONTEXT.as_bytes()),
                item(
                    item::USER_INFORMATION,
                    &[
                        item(item::MAX_PDU_LENGTH, &16384u32.to_be_bytes()),
                        item(item::SOP_CLASS_EXTENDED_NEGOTIATION, &value),
                    ]
                    .concat(),
                ),
            ],
        );
        match decode(&frame).expect("decode") {
            Pdu::AssociateRq(rq) => {
                let ext = &rq.user_information.extended_negotiations[0];
                assert_eq!(ext.sop_class, STORAGE_CLASS);
                assert_eq!(ext.requested, vec![0, 1, 4], "the flat array is field 1..=3");
            }
            other => panic!("expected AssociateRq, got {other:?}"),
        }

        // encode direction: three fields are three bytes
        let mut rq = Associate::new("SCP", "SCU");
        rq.user_information.extended_negotiations.push(ExtendedNegotiation {
            sop_class: STORAGE_CLASS.into(),
            requested: vec![1, 0, 1],
            accepted: Vec::new(),
        });
        let bytes = encode(&Pdu::AssociateRq(rq));
        let value = item_value(&user_items(&bytes), item::SOP_CLASS_EXTENDED_NEGOTIATION).expect("extended item");
        assert_eq!(value.len(), 2 + STORAGE_CLASS.len() + 3, "no per-field length prefixes");
        assert_eq!(&value[2 + STORAGE_CLASS.len()..], &[1, 0, 1]);
    }

    #[test]
    fn user_identity_is_type_positive_then_length_prefixed_fields() {
        // PS3.8 §D.3.3.7
        let mut value = vec![0x02, 0x01];
        value.extend_from_slice(&5u16.to_be_bytes());
        value.extend_from_slice(b"alice");
        value.extend_from_slice(&6u16.to_be_bytes());
        value.extend_from_slice(b"secret");
        let frame = associate_frame(
            0x01,
            &[
                item(item::APPLICATION_CONTEXT, APPLICATION_CONTEXT.as_bytes()),
                item(
                    item::USER_INFORMATION,
                    &[
                        item(item::MAX_PDU_LENGTH, &16384u32.to_be_bytes()),
                        item(item::USER_IDENTITY_RQ, &value),
                    ]
                    .concat(),
                ),
            ],
        );
        match decode(&frame).expect("decode") {
            Pdu::AssociateRq(rq) => {
                let identity = rq.user_information.user_identity.expect("identity");
                assert_eq!(identity.identity_type, 2);
                assert!(identity.positive_response_requested, "positive follows the type");
                assert_eq!(identity.primary, b"alice");
                assert_eq!(identity.secondary, b"secret");
            }
            other => panic!("expected AssociateRq, got {other:?}"),
        }

        // encode direction: the exact standard bytes
        let mut rq = Associate::new("SCP", "SCU");
        rq.user_information.user_identity = Some(UserIdentity {
            identity_type: 2,
            primary: b"alice".to_vec(),
            secondary: b"secret".to_vec(),
            positive_response_requested: true,
        });
        let bytes = encode(&Pdu::AssociateRq(rq));
        let value = item_value(&user_items(&bytes), item::USER_IDENTITY_RQ).expect("identity item");
        let expected = b"\x02\x01\x00\x05alice\x00\x06secret".to_vec();
        assert_eq!(&value[..expected.len()], &expected[..], "type, positive, then each length+value");
    }

    #[test]
    fn user_identity_server_response_is_length_prefixed_and_validated() {
        // PS3.8 §D.3.3.7: the AC item body is `[server-response-length u16][bytes]`.
        let mut response = Vec::new();
        response.extend_from_slice(&2u16.to_be_bytes());
        response.extend_from_slice(b"OK");
        let frame = associate_frame(
            0x02,
            &[
                item(item::APPLICATION_CONTEXT, APPLICATION_CONTEXT.as_bytes()),
                item(
                    item::USER_INFORMATION,
                    &[
                        item(item::MAX_PDU_LENGTH, &16384u32.to_be_bytes()),
                        item(item::USER_IDENTITY_AC, &response),
                    ]
                    .concat(),
                ),
            ],
        );
        match decode(&frame).expect("decode") {
            Pdu::AssociateAc(ac) => {
                assert_eq!(ac.user_information.user_identity_response.as_deref(), Some(&b"OK"[..]))
            }
            other => panic!("expected AssociateAc, got {other:?}"),
        }

        // a declared length past the item is malformed, not silently truncated
        let mut bad = Vec::new();
        bad.extend_from_slice(&9u16.to_be_bytes());
        bad.extend_from_slice(b"OK");
        let bad_frame = associate_frame(
            0x02,
            &[
                item(item::APPLICATION_CONTEXT, APPLICATION_CONTEXT.as_bytes()),
                item(
                    item::USER_INFORMATION,
                    &[item(item::MAX_PDU_LENGTH, &16384u32.to_be_bytes()), item(item::USER_IDENTITY_AC, &bad)]
                        .concat(),
                ),
            ],
        );
        assert!(decode(&bad_frame).is_err(), "an overlong server response is rejected");

        // encode direction: the item body carries the u16 length prefix
        let mut ac = Associate::new("SCP", "SCU");
        ac.user_information.user_identity_response = Some(b"OK".to_vec());
        let bytes = encode(&Pdu::AssociateAc(ac));
        let value = item_value(&user_items(&bytes), item::USER_IDENTITY_AC).expect("response item");
        assert_eq!(value, b"\x00\x02OK", "server-response-length precedes the bytes");
    }

    #[test]
    fn a_common_extended_length_that_overruns_the_item_is_an_error() {
        // 0x57: uid_len(2) + service class UID + (len(2) + related UID)*.
        // A length pointing past the item is malformed input, not a reason to
        // stop reading and report a partial list.
        let mut value = Vec::new();
        value.extend_from_slice(&4u16.to_be_bytes());
        value.extend_from_slice(b"1.2.");
        value.extend_from_slice(&9u16.to_be_bytes()); // claims 9 bytes, 5 remain
        value.extend_from_slice(b"1.2.3");
        let frame = associate_frame(
            0x01,
            &[
                item(item::APPLICATION_CONTEXT, APPLICATION_CONTEXT.as_bytes()),
                item(
                    item::USER_INFORMATION,
                    &[
                        item(item::MAX_PDU_LENGTH, &16384u32.to_be_bytes()),
                        item(item::SOP_CLASS_COMMON_EXTENDED, &value),
                    ]
                    .concat(),
                ),
            ],
        );
        match decode(&frame) {
            Err(NetworkError::Protocol(message)) => {
                assert!(message.contains("related SOP class"), "got {message}")
            }
            other => panic!("expected a Protocol error, got {other:?}"),
        }

        // the service class UID length is checked the same way
        let mut value = Vec::new();
        value.extend_from_slice(&100u16.to_be_bytes());
        value.extend_from_slice(b"1.2.3");
        let frame = associate_frame(
            0x01,
            &[
                item(item::APPLICATION_CONTEXT, APPLICATION_CONTEXT.as_bytes()),
                item(
                    item::USER_INFORMATION,
                    &[
                        item(item::MAX_PDU_LENGTH, &16384u32.to_be_bytes()),
                        item(item::SOP_CLASS_COMMON_EXTENDED, &value),
                    ]
                    .concat(),
                ),
            ],
        );
        match decode(&frame) {
            Err(NetworkError::Protocol(message)) => {
                assert!(message.contains("service class UID"), "got {message}")
            }
            other => panic!("expected a Protocol error, got {other:?}"),
        }
    }
}
