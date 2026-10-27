# radicom Development Plan and Progress

> Version: v0.1 (2026-10-27)
> Companion documents: [01-feature-list.md](01-feature-list.md), [02-architecture.md](02-architecture.md)
> This document tracks the milestone plan, current progress and risks. **Update the progress tables in §3 and the change log in §5 at the end of every development iteration.**

---

## 1. Milestone Overview

Milestones are ordered so that each one delivers a usable, testable increment. Feature IDs refer to the feature list.

| Milestone | Theme | Feature IDs covered | Detailed spec | Exit condition (acceptance) |
|---|---|---|---|---|
| **M0** | Project scaffolding | INF-05, INF-06 | [09-testing.md](09-testing.md) | Workspace builds; xtask generates the dict crate; CI runs fmt/clippy/test; test data imported |
| **M1** | Identifier system (Tag/VR/UID/TS/Dictionary) | TAG-01..05, VR-01..02, UID-01..04, TS-01..03, DICT-01..03, VM-01, INF-01 (partial), INF-02 | [04-data-model.md](04-data-model.md) §1–6 | All identifier types usable; generated tables complete; round-trip parse/display tests pass |
| **M2** | Dataset, elements, charset, validation | DS-01..10, ELEM-01..05, ENC-01..04, VAL-01..03 | [04-data-model.md](04-data-model.md) §7–11 | A dataset can be built programmatically, values read/written in all three API tiers, private tags work, multi-charset strings round-trip, validators pass fo-dicom-aligned cases |
| **M3** | File and IO | BUF-01..02, IO-01..03, RD-01..05, WR-01..03, FILE-01..06, MEM-01 (deferred to M8 if needed) | [05-file-io.md](05-file-io.md) | Real-world DICOM files (all variants from FILE-03) parse and write back byte-faithfully; large-object policies work; malformed-data corpus (tolerance rows T1–T10) passes |
| **M4** | Pixel data, codec interfaces, transcoder | PIX-01..05, CODEC-01..03, CODEC-05, TR-01..03 (~~CODEC-04~~ dropped: zero built-in codecs, all implementations incl. RLE live in radicom-codecs) | [06-pixel-codec.md](06-pixel-codec.md) | PixelData facade reads native frames and aggregates encapsulated frames via the offset table; Transcoder's full decision table verified end-to-end with the in-repo test-only mock codec; missing-codec paths fail fast with typed errors; **the codec boundary API is frozen for radicom-codecs** |
| **M5** | Network core | NET-01..03, NET-05..08, SRV-01..03, DIMSE-01, OBS-01 | [07-network.md](07-network.md) §1–8 | Loopback SCU↔SCP: C-ECHO and C-STORE succeed against a reference peer (e.g. DCMTK/fo-dicom); PDU golden tests pass |
| **M6** | Network services complete | SRV-04..08, CLI-01..02, NET-04 | [07-network.md](07-network.md) §4, 7, 9 | C-FIND/C-GET/C-MOVE against a reference PACS-like peer; DicomClient send loop with linger/retry/cancel; extended negotiation + user identity |
| **M7** | Serialization, media, SR, anonymization | JSON-01..02, XML-01, MEDIA-01..03, SR-01..02, ANON-01, INF-03..04 | [08-serialization-media.md](08-serialization-media.md) | DICOM JSON round-trips against fo-dicom-produced samples; DICOMDIR builds and re-reads; SR trees traverse; anonymizer applies the full PS3.15 profile |
| **M8** | Rendering, printing, enhancements (P2) | REND-01..11, PRINT-01..02, MATCH-01, XFORM-01, MEM-01, UID-04..05, SRV-08, HOST-01, OBS-02, INF-07 | [10-rendering.md](10-rendering.md); [08-serialization-media.md](08-serialization-media.md) §5; [07-network.md](07-network.md) §8.1 | Render a 16-bit CT frame to PNG correctly; printing models persist and layouts render; C-CANCEL works; hosting helper + OTel wiring + dump CLI delivered; benchmarks published |

Dependency chain: M0 → M1 → M2 → M3 → M4 → (M5 → M6) and (M7) can proceed in parallel after M4; M8 last.

## 2. Iteration Breakdown (Suggested)

Each milestone is split into coarse iterations of roughly 1–2 weeks of focused work. Adjust freely; the milestone exit conditions are what matter.

**M0 — Scaffolding**
1. Cargo workspace + crate skeletons (`radicom`, `radicom-dict`, `radicom-network`, `radicom-json`) + empty CI.
2. `xtask gen-dict`: parse fo-dicom dictionary XML → generate `radicom-dict` sources; commit artifacts.
3. Import test data (subset of fo-dicom test files, license-checked) into `tests-data/`; test harness helpers.

**M1 — Identifiers**
1. `Tag` (TAG-01/02), `PrivateCreator`, `MaskedTag` (TAG-05), `Vr` (VR-01/02).
2. `Uid` + registry + generator (UID-01..03); `TransferSyntax` + registry (TS-01/02); virtual syntaxes (TS-03).
3. Dictionary loading/lookup (DICT-01..03), `Vm` (VM-01); error hierarchy skeleton (INF-01) and implementation identity (INF-02).

**M2 — Dataset**
1. Element value model + typed conversion matrix (ELEM-01/02); fragment & sequence items (ELEM-03/04).
2. `Dataset` container: read tiers, write API, private tag operations, clone/copy (DS-01..05); walker (DS-09).
3. Charset module (ENC-01..04); validators + switches (VAL-01..03); lazy evaluation (ELEM-05); date-time helpers (DS-08), functional groups (DS-06), content equality (DS-07).

**M3 — File IO**
1. Byte buffers + sources/targets + endianness (BUF-01/02, IO-02/03).
2. Reader: recursive-descent parser + observers + tolerance rules (RD-01..05); `DicomReader`/file-level reader with format detection (FILE-03).
3. Writer + length calculator + group lengths (WR-01..03); `DicomFile` open/save + meta table + read options (FILE-01/02/04/05/06); file/directory references and temp files (IO-01).

**M4 — Pixel & codecs**
1. `PixelData` facade + frame access + size math (PIX-01..03); palette LUT + overlay access (PIX-04/05).
2. Codec trait + params + registry (empty default — no built-in codecs) + test-only mock codec (CODEC-01..03); JPEG bit-depth utility (CODEC-05, P2).
3. `Transcoder` + single-frame decode + convenience clones (TR-01..03); **boundary API review with radicom-codecs; freeze**.

**M5 — Network core**
1. TCP/TLS abstractions (NET-01); PDU codec (NET-02); association model (NET-03).
2. Protocol engine: receive loop, PDV reassembly, queues, streaming send, lifecycle (NET-05/06/08); DIMSE message model + status codes (DIMSE-01).
3. Server + C-ECHO SCP + C-STORE service (SRV-01..03); timeouts (NET-07); logging (OBS-01).

**M6 — Network services**
1. C-FIND (SRV-04); C-GET/C-MOVE (SRV-05/06).
2. DicomClient high level (CLI-01); expert layer (CLI-02); extended negotiation + user identity (NET-04).
3. N-services and C-CANCEL (SRV-07/08, P2 — schedule permitting).

**M7 — Serialization & media**
1. DICOM JSON (JSON-01/02); XML export (XML-01).
2. DICOMDIR (MEDIA-01..03); SR (SR-01/02).
3. Anonymizer (ANON-01); dump tooling + logging refinements (INF-03/04).

**M8 — Rendering & enhancements**
1. Rendering entry + pipelines + LUTs + render options (REND-01..04) — spec: [10-rendering.md](10-rendering.md).
2. Pixel render types + color conversion + image backends (REND-05..07).
3. Geometry/localizer/MPR (REND-08/09); icon rendering (REND-10); spatial transform + math utilities (REND-11); printing models + layout rendering (PRINT-01/02); match/transform DSLs (MATCH-01, XFORM-01); memory pool (MEM-01); Meta SOP mapping + network identifier (UID-04/05); C-CANCEL (SRV-08); hosting helper (HOST-01, [07-network.md](07-network.md) §8.1); OTel wiring (OBS-02); dump CLI (INF-07); benchmark suite + performance pass.

## 3. Progress Tracking

Status legend: `Not started` / `In progress` / `Done` / `Blocked` / `Deferred (P2)`.

### 3.1 Milestone status

| Milestone | Theme | Status | Notes |
|---|---|---|---|
| M0 | Scaffolding | Done | Workspace + 4 crates + xtask gen-dict committed (8e1be2a, fe25717, 8188021); build/tests/clippy green |
| M1 | Identifier system | Done | TAG/VR/UID/TS/DICT/VM/MaskedTag/tag-index committed (343bd56..16847ac); 5347-tag + 1928-UID generated tables verified by tests; exit condition met |
| M2 | Dataset & elements | Not started | |
| M3 | File & IO | Not started | |
| M4 | Pixel & codec interfaces | Not started | Boundary freeze gates radicom-codecs kickoff |
| M5 | Network core | Not started | |
| M6 | Network services | Not started | |
| M7 | Serialization & media | Not started | |
| M8 | Rendering & enhancements | Deferred (P2) | |

### 3.2 Feature-group status (fine-grained)

| Group | Features | Status | Notes |
|---|---|---|---|
| Tag | TAG-01..05 | Done | committed 343bd56, 16847ac |
| VR | VR-01..02 | Done | golden attribute table test; committed f3c9cff |
| UID | UID-01..03 | Done | registry seeded from generated table; committed e4905f9. UID-04/05 deferred to M8 |
| Transfer syntax | TS-01..03 | Done | 56 built-ins + virtual syntaxes; committed c6045f4 |
| Dictionary | DICT-01..03, VM-01 | Done | lookup precedence, XML load, one-shot default; committed f4fb7a8 |
| Dataset | DS-01..10 | Not started | |
| Elements | ELEM-01..05 | Not started | |
| Charset | ENC-01..04 | Not started | |
| Validation | VAL-01..03 | Not started | |
| Buffers | BUF-01..02 | Not started | |
| Byte source/target | IO-01..03 | Not started | |
| Reader | RD-01..05 | Not started | |
| Writer | WR-01..03 | Not started | |
| DicomFile | FILE-01..06 | Not started | |
| PixelData | PIX-01..05 | Not started | |
| Codec interface | CODEC-01..03, CODEC-05 | Not started | CODEC-04 dropped 2026-10-27 (no built-in codecs; all implementations incl. RLE in radicom-codecs) |
| Transcoder | TR-01..03 | Not started | |
| Network transport | NET-01..02 | Not started | |
| Association | NET-03..04 | Not started | NET-04 in M6 |
| Protocol engine | NET-05..08 | Not started | |
| Server & services | SRV-01..08 | Not started | SRV-07/08 in M8 |
| Client | CLI-01..02 | Not started | |
| DIMSE model | DIMSE-01 | Not started | |
| Observability | OBS-01..02 | Not started | OBS-02 (OTel wiring) deferred to M8 |
| JSON/XML | JSON-01..02, XML-01 | Not started | |
| Media (DICOMDIR) | MEDIA-01..03 | Not started | |
| Structured report | SR-01..02 | Not started | |
| Printing | PRINT-01..02 | Deferred (P2) | |
| Anonymizer | ANON-01 | Not started | |
| Rendering | REND-01..11 | Deferred (P2) | Spec: [10-rendering.md](10-rendering.md) |
| Hosting helper | HOST-01 | Deferred (P2) | Spec: [07-network.md](07-network.md) §8.1 |
| DSLs | MATCH-01, XFORM-01 | Deferred (P2) | |
| Infrastructure | INF-01..07 | In progress | INF-01 error hierarchy done; INF-05 codegen done (M0); INF-06 test data: generated-table tests done, corpus import pending in M3; INF-07 (dump CLI) in M8 |

### 3.3 Done list (most recent first)

| Date | Item | Milestone |
|---|---|---|
| 2026-10-27 | M1 identifier system complete: Tag/VR/UID/TransferSyntax/Dictionary/VM/MaskedTag/tag-index, 52 tests green, clippy clean | M1 |
| 2026-10-27 | M0 scaffolding: workspace, four crates, xtask gen-dict producing committed generated tables | M0 |

| Date | Item | Milestone |
|---|---|---|
| 2026-10-27 | Parity re-audit: every fo-dicom platform capability now has a planned Rust equivalent (HOST-01, OBS-02, PRINT-02, INF-07, UID-05 added); detailed rendering module spec written (10-rendering.md); fo-dicom absolute paths removed from docs | Planning |
| 2026-10-27 | Decision: radicom ships **zero built-in codecs** — CODEC-04 (built-in RLE) dropped; default codec registry is empty; all implementations incl. RLE belong to radicom-codecs; M4 acceptance re-based on the test-only mock codec | Planning |
| 2026-10-27 | Coverage audit: walked the full fo-dicom source-file inventory against the docs; added exclusions table (01 §0), REND-10/11, divergences D10–D12, corrected license references to MS-PL | Planning |
| 2026-10-27 | Module specs written: 00-overview, 04-data-model, 05-file-io, 06-pixel-codec (codec boundary contract), 07-network, 08-serialization-media, 09-testing | Planning |
| 2026-10-27 | fo-dicom source analysis completed (core model, IO/serialization, network, imaging/codec interfaces) | Planning |
| 2026-10-27 | Feature list, architecture design and this development plan written | Planning |

## 4. Risks and Mitigations

| # | Risk | Impact | Mitigation |
|---|---|---|---|
| R1 | Malformed-file tolerance rules (RD-03) are scattered fo-dicom folklore; missing one produces failures on real-world data | High | Build the tolerance corpus from fo-dicom's historical issue list during M3; fuzz the reader |
| R2 | Codec boundary API churn breaks radicom-codecs after the freeze | High | Boundary review in M4 with radicom-codecs stakeholders; semver discipline (§6 of the architecture doc) |
| R3 | Multi-charset (ISO 2022) encoding is intricate; naive implementations corrupt CJK names | High | Standalone charset module with fo-dicom-aligned unit tests (D9); test against real patient-name samples |
| R4 | Network spec details (async window, P-DATA circuit breaker, maxPDU interplay) cause interoperability bugs | Medium | Loopback tests against DCMTK/fo-dicom peers from M5 onward; PDU golden samples |
| R5 | DICOMDIR offset computation and record stitching are error-prone | Medium | Two-pass writer with golden files; nearest-offset tolerance tests |
| R6 | Dictionary XML licensing/version drift | Low | fo-dicom dictionaries are MIT; keep attribution; pin the standard version constant |
| R7 | Scope creep from P2 features delaying P0 completion | Medium | P2 items only start after M7; progress table keeps them visibly deferred |

## 5. Change Log

| Date | Author | Change |
|---|---|---|
| 2026-10-27 | radicom team | Parity re-audit: platform capabilities mapped to Rust equivalents (HOST-01/OBS-02/PRINT-02/INF-07/UID-05); rendering module spec added (10-rendering.md); absolute fo-dicom paths removed from all docs |
| 2026-10-27 | radicom team | Scope change: no built-in codecs in radicom — CODEC-04 dropped (RLE moves to radicom-codecs), default codec registry empty, divergence D10 strengthened, M4 acceptance re-based on the mock codec |
| 2026-10-27 | radicom team | Coverage audit against the complete fo-dicom file inventory: added exclusion table (01 §0), REND-10 (icon rendering) and REND-11 (spatial transform + math utils), divergences D10–D12; license references corrected to MS-PL |
| 2026-10-27 | radicom team | Added module specification series (docs 00, 04–09): detailed per-module designs incl. the parser tolerance corpus (T1–T10) and the normative codec boundary contract (06 §6) |
| 2026-10-27 | radicom team | Initial version: milestone plan from the fo-dicom analysis; progress baseline (planning complete, code not started) |

## 6. Conventions

- Update §3 tables whenever a feature group changes status; add an entry to §3.3 and §5 each iteration.
- When a milestone completes, record its exit-condition verification (test run summary or peerinterop notes) in §3.1 Notes.
- Feature IDs never change; superseded features are marked `Dropped` with a reason rather than deleted.
