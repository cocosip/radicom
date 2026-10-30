# Interop Testing Against Real Peers

> Current correction and evidence ledger: [07-review-fixes-2026-10-30.md](07-review-fixes-2026-10-30.md).
> Earlier DONE / coverage statements describe an earlier checkpoint, not proof of complete parity.

How to verify radicom against a **real DICOM peer** — a fo-dicom C-STORE SCP or Q/R SCP, DCMTK,
dcm4che, or a production PACS.

> The repository ships SCU/SCP loopback regressions covering C-ECHO/STORE/FIND/MOVE/GET, negotiation,
> TLS and cancellation. They exercise real local sockets. Cross-implementation acceptance still needs
> an external peer; the historical peer results below were not repeated in this repair round.

## 0. Offline self-test (runs by default)

The `self_interop_*` cases in `crates/radicom-network/tests/interop.rs` need no peer: they start a
`DicomServer` on `127.0.0.1` and drive real TCP with a `DicomClient`, verifying C-ECHO and a **real file
C-STORE over TCP → SCP → read back** with semantic equality (compared element by element after
normalising group lengths; the transfer syntax is preserved). `cargo test --workspace --all-features`
runs them.

## 1. Interop tool: the `peer_interop` example

Location: `crates/radicom-network/examples/peer_interop.rs`

It supports optional C-ECHO; optional C-STORE of a file; and optional C-FIND / C-GET / C-MOVE. **Each
phase uses its own association**, so a C-STORE-only SCP that does not support Verification does not
break the later operations. C-ECHO is fatal only when it is the sole requested operation; any
*requested* operation that fails exits non-zero.

```pwsh
# C-ECHO only
cargo run -p radicom-network --example peer_interop -- `
  --peer 127.0.0.1:11112 --calling RADICOM --called DCM4CHE

# C-STORE only (skips C-ECHO; for C-STORE-only SCPs)
cargo run -p radicom-network --example peer_interop -- `
  --peer 127.0.0.1:11112 --calling RADICOM --called DCM4CHE --no-echo `
  --store tests-data\explicit-le\GH487.dcm

# C-FIND (STUDY)
cargo run -p radicom-network --example peer_interop -- `
  --peer 127.0.0.1:11112 --calling RADICOM --called DCM4CHE --no-echo `
  --find STUDY --patient-name "Doe*"

# C-GET (STUDY): instances arrive as reverse C-STOREs
cargo run -p radicom-network --example peer_interop -- `
  --peer 127.0.0.1:11112 --calling RADICOM --called DCM4CHE --no-echo `
  --get STUDY --study-uid 1.2.3 --storage 1.2.840.10008.5.1.4.1.1.2

# C-MOVE (STUDY): the SCP must map --dest to this host's address
cargo run -p radicom-network --example peer_interop -- `
  --peer 127.0.0.1:11112 --calling RADICOM --called DCM4CHE --no-echo `
  --move STUDY --study-uid 1.2.3 --dest ARCHIVE
```

Arguments:

- `--peer host:port` (default `127.0.0.1:11112`)
- `--calling AE` / `--called AE`
- `--no-echo` — skip C-ECHO (use when the peer does not support Verification)
- `--store <file.dcm>` — C-STORE that file (SOP class taken from `(0008,0016)`)
- `--find PATIENT|STUDY|SERIES|IMAGE` — C-FIND
- `--get PATIENT|STUDY|SERIES|IMAGE` — C-GET; `--storage uid1,uid2` lists accepted storage classes
  (defaults to CT/MR/SC). A mismatch makes the sub-operations fail, so set this to the actual instance
  SOP classes
- `--move PATIENT|STUDY|SERIES|IMAGE --dest AE` — C-MOVE; `--dest` is the AE the SCP must deliver to
- FIND/GET/MOVE share the match keys: `--patient-name`, `--patient-id`, `--study-uid`, `--accession`

## 2. Gated interop tests (optional in CI)

Location: `crates/radicom-network/tests/interop.rs`

A normal `cargo test` **skips** them (no peer, keeping the suite self-contained). They connect for real
only when `RADICOM_PEER` is set:

```pwsh
$env:RADICOM_PEER    = "127.0.0.1:11112"
$env:RADICOM_CALLING = "RADICOM"
$env:RADICOM_CALLED  = "DCM4CHE"
$env:RADICOM_STORE_FILE = "tests-data\explicit-le\GH487.dcm"   # optional C-STORE
cargo test -p radicom-network --test interop -- --nocapture
```

## 3. Acceptance criteria against fo-dicom

| Service | Peer | Acceptance |
|---|---|---|
| C-ECHO | Verification SCP | status 0x0000 (a C-STORE-only SCP may not support it — that is normal) |
| C-STORE | C-STORE SCP | status 0x0000; the peer's dataset is semantically equal to the source |
| C-FIND | Q/R SCP | correct match count and return keys (PatientName / StudyInstanceUID, ...) |
| C-MOVE | Q/R SCP + destination AE | correct completed sub-operation count; instances land at the destination |
| C-GET | Q/R SCP | correct completed reverse-C-STORE count; the reverse-store handler receives the instances |

> C-MOVE and C-GET need a destination-AE / reverse-store endpoint. radicom can play that role itself
> (`collect_reverse_stores` plus `DicomServer`), which the loopback suite already covers.

## 4. Measured results against a real fo-dicom peer

Measured 2026-10-28 against SCPs implemented with fo-dicom, after the AE configuration was corrected:

| Service | Peer | Called / Calling | Result |
|---|---|---|---|
| C-ECHO | Q/R SCP `172.16.2.24:31803` | QRGATE / NETPUSH | ✅ status 0x0000 |
| C-STORE | C-STORE SCP `172.16.2.24:31801` | NETGATE / NETPUSH | ✅ status 0x0000 (SC `…1.1.7`, CT `…1.1.2`) |
| C-FIND | Q/R SCP `:31803` | QRGATE / NETPUSH | ✅ PATIENT 18 / STUDY 21 / SERIES 1 / IMAGE 1 results, final status 0x0000 |
| C-GET | Q/R SCP `:31803` | QRGATE / NETPUSH | ✅ one study returned 1 CT; another returned **52 CTs**, sub-ops ok=52 failed=0, final 0x0000 |
| C-MOVE | Q/R SCP `:31803` | QRGATE / NETPUSH | ⏸️ not tested: the peer did not map the destination AE to this host (a peer-side network configuration issue). The function is covered by loopback; retest when the environment is ready |

> The C-STORE SCP does not accept Verification (C-ECHO returns SopClassNotSupported), which is normal for
> that class of SCP.

## 5. DUL conformance problems found by real-peer testing

These bugs were completely self-consistent under loopback plus self-generated goldens, and only a **real,
strict peer** exposed them. All are fixed per PS3.8 with regression tests:

| # | Problem | Standard | Fix |
|---|---|---|---|
| 1 | A-ASSOCIATE items were written as `type(1) + length(4)` | PS3.8 §9.3.2: `type(1) + reserved(1) + length(2)` — a 16-bit item length; only a P-DATA PDV uses 32 bits | `push_item` / `item_header` now write the 4-byte header; P-DATA unchanged |
| 2 | Odd-length item values were not padded to even | §9.3: an item value must be even length; pad an odd value with one `0x00` | the encoder pads; the decoder strips trailing NUL/space |
| 3 | A-ASSOCIATE-AC re-encoded the abstract syntax | §9.3.3: an AC returns only `result` plus **one** accepted transfer syntax, with no abstract syntax | the AC omits the abstract syntax; the client refills it from its RQ by context ID |
| 4 | The A-ASSOCIATE-RJ body was missing a reserved byte | the standard RJ body is `reserved(1) + result(1) + source(1) + reason(1)` | the encoder writes the reserved byte; the decoder skips it and validates the 4-byte length |

## 6. Offline consistency with fo-dicom's corpus

`tests-data/` is a corpus imported from fo-dicom's MS-PL test data. `crates/radicom/tests/corpus.rs`
round-trips 23 samples through **parse → write → parse** with semantic equality; `goldens.rs` compares
dumps, JSON and association/P-DATA PDU hex byte for byte or structurally. To expand it, drop
fo-dicom-generated `.dcm` files into the matching `tests-data/<category>/` directory and register them in
the `CORPUS` table in `corpus.rs`.
