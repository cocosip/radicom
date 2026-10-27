# Module Spec: Network (DIMSE) — radicom-network

> Status: in progress (milestones M5–M6; see [03-development-plan.md](03-development-plan.md) §3) — PDU codec, DIMSE model, association negotiation, PDV assembly, engine state and a working TCP C-ECHO are implemented; the streaming send loop, C-STORE sink, C-FIND/C-GET/C-MOVE, TLS and the client layers remain
> Feature IDs covered: NET-01..08, SRV-01..08, CLI-01..02, DIMSE-01, OBS-01
> Behavior baseline: fo-dicom `Network/` (DicomService, PDU.cs, DicomAssociation, DicomServer, DicomClient + Advanced layer, DicomMessage/service types)
> Runtime: tokio; all APIs `async`.

---

## 1. Layering and Task Model

```
 DicomServer (listener)                DicomClient (queue + association manager)
      │ per-connection task                 │ per-association over one connection
      ▼                                     ▼
 DicomService  ←── one protocol engine per TCP/TLS connection ──►  DicomService
      │ PDU read loop │ PDU write queue (single writer)
      ▼
 RawPdu codec ── TCP (tokio) / TLS (tokio-rustls)
      │
      ▼ (within DicomService)
 DIMSE assembler (PDV → messages)  ·  DIMSE dispatcher (provider traits / pending table)
```

- **One connection = one `DicomService` task** owning: the socket halves, a PDU send queue (bounded, backpressure), the pending-request table, and the negotiated association state.
- SCU and SCP are roles of the same engine, selected by which side initiated; provider traits are called only on the SCP side.

## 2. Transport (NET-01)

- `TcpStream` with: `nodelay` (default on), configurable send/receive buffer sizes, connect timeout, read/write timeouts.
- TLS: `TlsInitiator` (client) / `TlsAcceptor` (server) traits over rustls config builders. Options: server certificate from PEM file or OS store (by subject), client certificate set, minimum protocol version (default TLS 1.2; 1.3 preferred), optional mutual authentication requirement, handshake timeout, custom verifier hook. The DICOM engine is TLS-agnostic — it sees an async stream.
- IPv6: bind/connect strings support `::`/`::1` natively.
- Host identity helper (UID-05): `network_identifier() -> Option<Uid>` deriving a UID from the primary MAC address (machine identity, fo-dicom `TryGetNetworkIdentifier`); pure utility, no runtime dependency on it.

## 3. PDU Layer (NET-02)

### 3.1 Common frame

Every PDU: `type(1) reserved(1)=0 length(4, big endian)`. Total PDU size ≤ 65535 bytes. Reader: read 6-byte header, validate type ∈ 0x01..0x07 (else `Error::Network(UnknownPdu)`), read `length` bytes (chunked), decode.

### 3.2 A-ASSOCIATE-RQ (type 0x01)

```
protocol-version(2)=0x0001  reserved(2)
called-ae(16, space padded)  calling-ae(16, space padded)
reserved(32)
application-context item:  0x10 len(4) "1.2.840.10008.3.1.1.1"
presentation-context item: 0x20 len(4) id(1,odd 1..255) reserved(1) reserved(1) reserved(1)
                          ├─ abstract-syntax item 0x30 len(4) uid
                          └─ transfer-syntax item(s) 0x40 len(4) uid
user-information item:     0x50 len(4)
  0x51 max-pdu-length(4)             0x52 implementation-class-uid
  0x53 async-ops (invoked,performed) — only when ≠ (1,1)
  0x54 scp-scu role selection (sop-class-uid, scu-role, scp-role)
  0x55 implementation-version-name   0x56 sop-class-extended-negotiation (sop-uid + raw)
  0x57 sop-class-common-extended (RQ only)   0x58 user-identity  0x59 user-identity server-response (AC only)
```

AE titles are trimmed of trailing spaces/`\0` on read; unknown user items are skipped by length. A-ASSOCIATE-AC (0x02) mirrors the RQ with PC item type 0x21 carrying `result(1)` and, when accepted, exactly one transfer syntax; rejected PCs carry Implicit VR LE as the syntax placeholder.

### 3.3 Other PDUs

- A-ASSOCIATE-RJ (0x03): `result(1: 1=permanent,2=transient) source(1: 1=user,2=provider-ACSE,3=provider-presentation) reason(1)` with the standard reason code sets per source.
- P-DATA-TF (0x04): `pdv-count... ` — actually a length-delimited series of PDVs: `pdv-length(4) pc-id(1) msg-header(1: bit0=command, bit1=last) value(padded to even)`.
- A-RELEASE-RQ (0x05) / A-RELEASE-RP (0x06): 4 reserved bytes.
- A-ABORT (0x07): `reserved(2) source(1: 0=user,1=unknown→provider,2=provider) reason(1)`.

## 4. Association Negotiation (NET-03/04)

### 4.1 Presentation contexts

- IDs: odd, assigned 1,3,5... (per-association sequence).
- States: `Proposed → Accept | RejectUser | RejectNoReason | RejectAbstractSyntaxNotSupported | RejectTransferSyntaxesNotSupported`.
- SCP-side `accept_syntaxes(candidates, priority)`: pick the first candidate present in the offered list (priority order); none → reject the PC (abstract syntax stays unsupported only if it never matched).
- Role selection pairs `(abstract, scu-role, scp-role)` are keyed per PC (C-STORE storage-class SCP role handling).
- Dedup on add: same abstract + same transfer syntax set + same roles ⇒ reuse.

### 4.2 Automatic PC generation from requests (fo-dicom-compatible rules)

- C-STORE: one PC per (abstract, source syntax) — the file's syntax plus any `additional_transfer_syntaxes`; force-include Implicit VR LE (omittable for lossy-only data via an explicit flag, otherwise an error); Deflated sources additionally offer Explicit VR LE.
- Other requests: user-provided PC or the Meta-SOP expansion with Implicit VR LE.
- `DicomMessage` send picks: accepted PC with matching accepted syntax (C-STORE) → else first accepted PC of the abstract syntax → else synthesize a local `SopClassNotSupported` failure (never emits an unusable message).

### 4.3 Extended negotiation (NET-04)

- `ExtendedNegotiation { sop_class, requested: AppInfo, accepted: Option<AppInfo> }`; `AppInfo` = ordered `(field_no, bytes)` map with a curated API for storage (level-of-support / digital-signature / element-coercion), C-FIND (relational-queries, datetime-matching, fuzzy-matching, timezone-adjustment, enhanced-multiframe-conversion) and C-GET/C-MOVE (relational-retrieval, enhanced-multiframe-conversion).
- Common extended: service-class UID + related general SOP classes (RQ only).
- User identity: types 1–5 (username, username+passcode, Kerberos, SAML, JWT); validation rules (secondary field only for type 2); positive response requested flag; server response surfaced on the AC.

## 5. Protocol Engine (`DicomService`, NET-05..08)

### 5.1 Receive loop

Loop: read PDU → dispatch:
- `A_ASSOCIATE_RQ` → build association → `provider.on_association_request(assoc) -> Accept(pcs, extended) | Reject(result,source,reason)`.
- `A_ASSOCIATE_AC/RJ` → complete/reject the association future.
- `P_DATA_TF` → PDV reassembly (5.2).
- `A_RELEASE_RQ` → `provider.on_release_request()` (default: agree) → send RP.
- `A_RELEASE_RP` → complete release, close.
- `A_ABORT` → callbacks, close.
Read of 0 bytes (peer closed) → forced close path.

### 5.2 PDV reassembly and DIMSE assembly

- Append PDV payload to the current message stream keyed by (pc-id, is-command); `last` bit completes a unit.
- Command set: parse as dataset with **Implicit VR Little Endian** always.
- Dispatch on `CommandField(0000,0100)` → instantiate one of the 22 message types (requests bit15=0; responses bit15=1).
- `CommandDataSetType(0000,0800)` `0x0101` ⇒ no dataset; anything else ⇒ continue assembling the dataset PDVs decoded with the PC's accepted transfer syntax; parsing runs with validation off (read-tolerant policy).
- C-STORE receive sink: default = spool to temp file, then `DicomFile::open` on it (streaming; memory bounded); the sink is injectable (SCP-side option). Parse failure ⇒ respond `ProcessingFailure` with truncated error comment (≤ LO length) and invoke `on_c_store_error`; connection survives.
- Response routing: match `(0000,0120) MessageIDBeingRespondedTo` against the pending table → wake the per-request channel; `Pending` status keeps the entry; final status removes it and completes the caller.

### 5.3 Sending (NET-06)

- A `PduStream` writer wraps a `ByteTarget`: the writer emits DIMSE bytes and the wrapper splits them into PDVs (command stream first as its own P-DATA-TF, then the dataset stream), each PDV even-padded, honoring `min(peer max pdu, local max data buffer)`; `max_pdvs_per_pdu` option (1 for buggy peers).
- Before sending a dataset: if its internal syntax ≠ PC accepted syntax → transcode via the global registry; unsupported change with pixel data ⇒ policy `IgnoreUnsupportedTransferSyntaxChange` (send as-is) or **strip pixel data + warn** (default mirrors fo-dicom).
- Command group length `(0000,0000)` recomputed; dataset group lengths removed per PS3.5 7.2.
- Single-writer discipline: all sends enqueue; one task drains.
- Backpressure: bounded PDU queue (default 16); when full, senders await (peer-liveness check every 60 s).
- **P-DATA circuit breaker**: if a request times out while PDVs are still queued, the engine stops accepting new P-DATA for that message and flags the connection `cannot_process_pdata`; the client reacts by rolling a new association.

### 5.4 Timeouts (NET-07)

Per-request watchdog (opt-in, default off) scanning the pending table: timeout = configured `request_timeout` measured against `max(last pending response, last PDU sent, enqueue)` timestamps. On timeout: emit timeout event, drop pending, maybe trip 5.3's breaker.

### 5.5 Connection close (NET-08)

Graceful: `try_close()` refuses while queues are non-empty (returns would-block; logged); forced: clear queues, fail all pending with `ClosedPrematurely`, notify provider (`on_connection_closed(err)`), drop the socket. All errors propagate to the owning task and surface through the client/server API.

## 6. DIMSE Message Model (DIMSE-01)

### 6.1 Command elements

| Tag | Element | Notes |
|---|---|---|
| (0000,0000) | CommandGroupLength | UL, recomputed on send |
| (0000,0002) | AffectedSOPClassUID | request: affected; response: echoed |
| (0000,0003) | RequestedSOPClassUID | N-* only |
| (0000,0100) | CommandField | US, 22 values (0x0001..0x0150 + responses 0x8xxx; C-CANCEL 0x0FFF reserved) |
| (0000,0110) | MessageID | US, per-association sequence (divergence D3) |
| (0000,0120) | MessageIDBeingRespondedTo | US |
| (0000,0600) | MoveDestination | AE, C-MOVE only |
| (0000,0700) | Priority | US 0/1/2 (low/high/medium per standard numeric order) |
| (0000,0800) | CommandDataSetType | 0x0101 = none |
| (0000,0900) | Status | US |
| (0000,0902) | ErrorComment | LO |
| (0000,1000..1030) | AffectedSOPInstanceUID / requested / N-* action/Event info | per service |

### 6.2 Status codes

~60 curated constants (generic, storage, query/retrieve, print, media creation). `Status { code, state: Success|Pending|Cancel|Warning|Failure, comment }`. Matching supports **mask classes** (e.g. `B000`-class warnings `Bxxx`) resolved best-match by most mask bits; unknown codes fall back to `ProcessingFailure` classification. Extensible via `add_known_statuses`.

### 6.3 Service request/response types

Per service: request struct (typed fields + convenience constructors), response struct (status + service-specific counters), provider trait (§7). C-FIND/C-GET/C-MOVE responses stream via `BoxStream`; C-STORE/N-EVENT on the SCU side register handler callbacks.

## 7. Services (SRV-01..08)

### 7.1 C-ECHO (SRV-02)

`CEchoProvider::on_c_echo(assoc, req) -> CEchoResponse`. Built-in `VerificationProvider` accepts only the Verification abstract syntax, answers Success. Server rejects unknown abstract syntaxes with the standard PC-level reject.

### 7.2 C-STORE (SRV-03)

- Request: `file: DicomFile` (or dataset + explicit SOP instance UID/syntax), `additional_transfer_syntaxes`, `omit_implicit_vr`, priority.
- SCP receive: spool → open file → `on_c_store(req) -> CStoreResponse`; exceptions map to status (`ErrorComment` truncated to LO).
- SCU convenience: `client.send_c_store(file)`.

### 7.3 C-FIND (SRV-04)

- `CFindRequest::patient_query/study_query/series_query/image_query/worklist_query(fills)` set Level + return-keys presets.
- Level → SOP class mapping: Patient/Study root selections, MWL class.
- SCP: `on_c_find(req) -> Stream<CFindResponse>`; engine sends each item with Pending and the final one with Success/Refused/Failed; each response carries Remaining/Completed/Failed counters where meaningful.

### 7.4 C-GET (SRV-05)

- Constructors per level; the SCP delivers instances as **C-STORE sub-operations on the same association** — the SCU must register an `on_c_store` handler at client construction (missing ⇒ `Error::Network(ReverseStoreHandlerMissing)`).
- Responses: sub-operation counters + status.

### 7.5 C-MOVE (SRV-06)

- `destination_ae` required; the SCP initiates a **new association** toward the destination (server-side outbound store client — provided by radicom-network as a helper) and sets MoveOriginatorAETitle/MessageID on the sub-operation C-STOREs (divergence D5: always written).
- Response stream: pending progress + final counters.

### 7.6 N-services and C-CANCEL (SRV-07/08, P2)

- N-EVENT-REPORT/N-GET/N-SET/N-ACTION/N-CREATE/N-DELETE messages + provider traits (storage-commitment shape).
- `client.cancel(message_id)` sends C-CANCEL (0x0FFF) for the in-flight request; engine routes to the provider stream to stop iteration.

## 8. Server (SRV-01)

```rust
pub struct DicomServer<P: ServiceProvider> { /* listener, registry, options */ }
pub struct ServerOptions { pub max_clients: usize /* 0 = unlimited */, pub tls: Option<TlsAcceptor>,
                           pub service: ServiceOptions }
impl<P> DicomServer<P> {
    pub async fn bind(addr, provider: Arc<P>, options) -> Result<Self>;  // one-shot; re-bind after shutdown allowed (D4)
    pub fn is_listening(&self) -> bool; pub fn connected_clients(&self) -> usize;
    pub async fn shutdown(self);   // graceful: stop accept, close idle, drain
}
```

- Accepted connections spawn handler tasks (bounded by `max_clients` semaphore; waiters logged periodically).
- A per-address registry prevents double-binding the same `ip:port` in-process.
- Provider construction per connection: user supplies `Arc<P>` (stateless providers) or a factory trait for per-connection state (`ServiceProviderFactory`).

### 8.1 Hosting helper (HOST-01, P2)

A thin, framework-agnostic lifecycle wrapper mirroring what fo-dicom's AspNetCore package gives .NET hosts:

```rust
pub struct HostedDicomServer<P: ServiceProvider> { /* config, server handle */ }
pub struct HostedServerConfig { pub port: u16, pub ae_title: String, pub tls: Option<TlsAcceptor>,
                                pub max_clients: usize, pub store_sink: Option<PathBuf> }
impl<P> HostedDicomServer<P> {
    pub fn new(config: HostedServerConfig, provider: P) -> Self;
    pub async fn start(&self) -> Result<()>;      // binds; preloads the dictionary; idempotent guard
    pub async fn stop(&self) -> Result<()>;       // graceful drain
    pub fn health(&self) -> ServerHealth;         // listening, connected_clients, last_error
    // general-purpose C-STORE SCP mode: saves instances under store_sink, emits
    // on_instance_received(file) events — the equivalent of GeneralPurposeDicomService
}
```

Applications embed this in their own runtime (tokio task, systemd, container); no web-framework coupling.

## 9. Client (CLI-01/02)

### 9.1 High level `DicomClient`

```rust
pub struct DicomClient { host, port, calling_ae, called_ae, tls: Option<TlsInitiator>, options: ClientOptions }
pub struct ClientOptions { association_request_timeout: 5s, association_release_timeout: 10s,
                           linger: 50ms, max_requests_per_association: Option<usize>,
                           max_consecutive_timeout_retries: 3, connection_timeout, cancellation_mode }
```

`send(requests)` loop (state machine):

```
IDLE → CONNECT → REQUEST-ASSOC ─(timeout: retry ≤N else Err)→ ASSOCIATED
     → SEND (parallel up to negotiated max-async-ops-invoked; drain-wake as each completes;
             keep pulling newly queued requests; breaker trip ⇒ move remainder to retry)
     → LINGER (wait `linger` for new requests; if some arrive and quota remains → back to SEND)
     → RELEASE (timeout → abort) → IDLE (retry batch loops) → done
```

- Events: `on_association_accepted/rejected/released`, `on_request_timeout` (per request, non-fatal).
- User identity validation: positive-response required but server returned none ⇒ error.
- Cancellation: `ImmediatelyReleaseAssociation` (default) or `ImmediatelyAbortAssociation`.

Convenience: `send_c_echo()`, `send_c_store(file)`, `send_c_find(req) -> Stream<CFindResponse>`, `send_c_move(req)`; reverse handlers `on_c_store`/`on_n_event_report` registered at construction (required for C-GET).

### 9.2 Expert layer

`Connection::open(opts) -> Connection` (one association per connection, enforced), `connection.open_association(AssociationRequest) -> Association`, `association.send(req) -> Stream<Response>`, `association.release()/abort()`, event stream accessor for connection-level events. The high-level client is implemented atop this layer (mirrors fo-dicom's AdvancedDicomClient*).

## 10. Observability (OBS-01)

- `tracing` spans: `dicom_conn{remote,calling,called}` → `dicom_assoc{id}` → `dicom_req{msg_id, sop_class}`; PDU-level debug logging opt-in (`log_data_pdus`, `log_dimse_datasets`).
- Optional `MetricsCollector` trait (bytes sent/received, connections opened/closed, associations) pluggable at server/client construction.

## 11. Errors

`NetworkError::{ Reject{result,source,reason}, Abort{source,reason}, AssociationRequestTimedOut{attempts}, RequestTimedOut{message_id}, ClosedPrematurely{source: Option<Error>}, Protocol(&'static str), Io(io::Error) }`. Every failure mode of §5 maps onto this set; nothing escapes as a generic IO error when a specific variant applies.

## 12. Test Requirements Checklist

- [ ] PDU codec: golden byte vectors for all 7 types both directions; unknown item skipping; 65535 limit; PDV even padding.
- [ ] Negotiation: PC result matrix; role selection; async ops item presence; user identity type validation.
- [ ] Engine loopback: echo/store/find/get/move against an in-repo SCP and against a reference peer (DCMTK storescp/fo-dicom) in CI (nightly).
- [ ] Streaming send: dataset larger than max PDU; max_pdvs_per_pdu=1 mode; breaker trip and client roll-over.
- [ ] Timeouts: request watchdog timestamps; association retry counting.
- [ ] Client: linger reuse (assert single association for bursty queue), cancellation modes, reverse-store handler requirement.
- [ ] TLS: self-signed mutual auth loopback; handshake timeout.
- [ ] Fuzz: random PDUs must not panic the engine (bounded parse, typed errors).
