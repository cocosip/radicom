# radicom Documentation

Three documents, one per question: what the code must obey, what is left to do, and how to prove it
against a real system. Start with the architecture document before changing code. Usage examples live in
the root [README.md](../README.md).

## Start Here

| Document | Read it when |
|---|---|
| [01-architecture.md](01-architecture.md) | **You are about to change code.** Invariants: tag ordering, tolerance rules T1–T10, the frozen codec boundary, wire formats, naming, intentional divergences |
| [02-development-guide.md](02-development-guide.md) | You need the feature list with stable IDs, milestone status, outstanding work, known limits, risks, testing, CI or versioning |
| [03-interop.md](03-interop.md) | You are testing against a real DICOM peer |

## Authority

**The code is the source of truth.** These documents record intent, invariants and known limits. When a
document disagrees with the implementation, check which is correct against PS3.3/PS3.5/PS3.7/PS3.8 and
fo-dicom's behaviour, then fix the one that is wrong — including the document.

[01-architecture.md](01-architecture.md) §15 records deviations from the intended design; there are none
at present. Anything listed there is a defect, not a contract.

Outstanding work, environment- or scope-limited items and deliberate ceilings are all in
[02-development-guide.md](02-development-guide.md) §3.

---

## Project Background

radicom is a Rust library for DICOM processing and encoding/decoding, functionally modeled on
**fo-dicom** (Fellow Oak DICOM, the most widely used .NET DICOM library). It is organized as a small
workspace:

- **radicom** — the core library: DICOM data model, file reading/writing, serialization, media files,
  structured reports, printing data models, anonymization, pixel data access, rendering, codec
  **interfaces** and the transcoder.
- **radicom-network** — tokio-based DIMSE networking (C- and N-services, SCU/SCP, TLS).
- **radicom-json** — DICOM JSON (PS3.18) and Native XML.
- **radicom-dict** — generated tag/UID/private dictionary tables.
- **radicom-codecs** (a separate project) — the image codec integration package (JPEG, JPEG-LS,
  JPEG 2000, HTJ2K and other native codecs), mirroring the relationship between `FO-DICOM.Core` and its
  external `fo-dicom.Codecs` package.

**Codec implementations are explicitly out of scope for this repository** — there are zero built-in
codecs (divergence D10). The platform facilities fo-dicom reaches through .NET are either implemented
here (server hosting helper, metrics emission, dump CLI, print layout rendering) or deliberately absent
(the WPF GUI, four imaging backends, a second JSON backend) — see the divergences in
[01-architecture.md](01-architecture.md) §14.

## Goals

1. **Feature parity with fo-dicom** for the in-scope areas, including its real-world tolerance behaviours
   (malformed files, private tag quirks, multi-charset strings), not just the happy path.
2. **Rust-idiomatic APIs**: traits, generics and ownership instead of DI containers and inheritance;
   `Result`/`Option` instead of exception tiers; zero-copy buffer views for large values.
3. **A clean, frozen codec boundary** so radicom-codecs can be developed independently.
4. **Testable against reality**: a tolerance corpus, round-trip baselines, and network interop tests
   against reference peers.

## Baseline and References

| Item | Value |
|---|---|
| Reference implementation | fo-dicom (Fellow Oak DICOM), v5.2.x stable / 6.0.0-alpha dev source tree |
| DICOM standard edition tracked | 2025d (per the fo-dicom ChangeLog) |
| Bundled dictionary version | 2026b (5347 public tags, 4679 private tags across 232 creators) |
| Primary standard parts | PS3.3 (information model), PS3.5 (data structures/encoding), PS3.6 (data dictionary), PS3.7 (message exchange), PS3.8 (network communication), PS3.10 (media storage), PS3.15 (security profiles), PS3.18 (web services / JSON) |

Feature IDs (`TAG-01`, `RD-03`, `NET-06`, ...) and tolerance rule IDs (`T1`–`T10`) are stable identifiers
defined in [02-development-guide.md](02-development-guide.md) §2.

## Glossary

| Term | Meaning |
|---|---|
| **AE (title)** | Application Entity title: up to 16 bytes identifying a DICOM network endpoint |
| **Attribute** | A tagged data element (tag + VR + value) |
| **Command set** | The DIMSE command dataset, always encoded Implicit VR Little Endian |
| **DICOMDIR** | A DICOM file of the Media Storage Directory SOP class indexing a file set |
| **DIMSE** | DICOM Message Service Element: C-ECHO/C-STORE/C-FIND/C-GET/C-MOVE and the N-services |
| **Encapsulated** | Pixel data stored as a fragment sequence (compressed transfer syntaxes) |
| **Fragment** | One item of an encapsulated pixel data sequence; the first conventionally holds the basic offset table |
| **Group length** | The `(GGGG,0000)` element giving its group's encoded length; required in file meta, legacy in datasets |
| **Item / Item delimiter** | `FFFE,E000` markers delimiting sequence items; a sequence may use undefined length with `FFFE,E0DD` |
| **PDU / PDV** | Protocol Data Unit / Presentation Data Value: the network wire units |
| **Preamble** | The 128-byte file prefix followed by `DICM` |
| **Presentation context** | An (abstract syntax, transfer syntax list) pair negotiated per association, identified by an odd byte ID |
| **Private creator** | The LO element at `(odd group, 00xx)` naming the owner of a private block |
| **SCU / SCP** | Service Class User (client) / Service Class Provider (server) |
| **SOP class / instance** | Service-Object Pair class (the UID-typed service+object type) / a concrete instance thereof |
| **SQ** | Value Representation "sequence of items" |
| **Transfer syntax** | The UID defining byte encoding rules (VR explicitness, endianness, compression) |
| **VR** | Value Representation: the 34 data types of DICOM attributes |
| **VM** | Value Multiplicity: how many values an attribute may hold ("1", "1-n", "2-2n") |

## Conventions

- Design changes discovered during implementation flow **back** into
  [01-architecture.md](01-architecture.md) in the same commit as the code — these documents are living,
  not one-shot artifacts.
- Feature IDs are never renumbered. A superseded feature is marked `Dropped` with a reason.
- Source comments reference sections as `docs/<file>.md §N`. Moving or renumbering a document means
  updating those references; `grep -r "docs/" crates xtask` finds them all.
- These documents describe the current state only. Review ledgers, parity audits and per-round evidence
  are historical and live in `git log`, not here.
