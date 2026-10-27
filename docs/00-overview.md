# radicom Project Overview

> Version: v0.1 (2026-10-27)
> This document gives the project background, goals, scope, terminology and a reading guide for the documentation series. Start here.

## 1. What is radicom

radicom is a DICOM processing and encoding/decoding library for Rust, functionally modeled on **fo-dicom** (Fellow Oak DICOM, the most widely used .NET DICOM library). It is organized as a small workspace:

- **radicom** (this repository) — the core library: DICOM data model, file reading/writing, network communication (DIMSE), serialization, media files, structured reports, printing data models, anonymization, pixel data access, codec **interfaces** and the transcoder.
- **radicom-codecs** (a separate project) — the image codec integration package (JPEG, JPEG-LS, JPEG 2000, HTJ2K and other native codecs), mirroring the relationship between fo-dicom's `FO-DICOM.Core` and its external `fo-dicom.Codecs` package.

**Codec implementations are explicitly out of scope for this repository** (they live in radicom-codecs; the core ships zero built-in codecs — divergence D10). Everything else fo-dicom's platform packages offer is planned with a Rust-native shape: server hosting helper (HOST-01), OpenTelemetry wiring (OBS-02), dump CLI (INF-07), print layout rendering (PRINT-02); only the .NET-specific packaging itself (AspNetCore-style hosting packages, Newtonsoft JSON backend, WPF GUI, four imaging backends) is not carried over — see the mapping table in [01-feature-list.md](01-feature-list.md) §0 and divergences D7/D10–D12 in [02-architecture.md](02-architecture.md) §12.

## 2. Goals

1. **Feature parity with fo-dicom** for the in-scope areas, including its real-world tolerance behaviors (malformed files, private tag quirks, multi-charset strings), not just the happy path.
2. **Rust-idiomatic APIs**: traits + generics + ownership instead of DI containers and inheritance; `Result`/`Option` instead of exception tiers; zero-copy buffer views for large values.
3. **A clean, frozen codec boundary** so radicom-codecs can be developed independently (see [06-pixel-codec.md](06-pixel-codec.md)).
4. **Testable against reality**: a tolerance corpus, round-trip baselines and network interop tests against reference peers.

## 3. Baseline and References

| Item | Value |
|---|---|
| Reference implementation | fo-dicom (Fellow Oak DICOM), v5.2.x stable / 6.0.0-alpha dev source tree |
| DICOM standard edition tracked | 2025d (per fo-dicom ChangeLog) |
| Bundled dictionary version | 2026b (public dictionary ~5347 tags; private dictionary ~4678 tags / 235 creators) |
| Primary standard parts | PS3.5 (data structures/encoding), PS3.6 (data dictionary), PS3.7 (message exchange), PS3.8 (network communication), PS3.10 (media storage), PS3.15 (security/confidentiality profiles), PS3.18 (web services / JSON) |

The feature IDs used across all documents (e.g. `TAG-01`, `RD-03`, `NET-06`) are defined in [01-feature-list.md](01-feature-list.md).

## 4. Documentation Map

| Document | Audience / purpose |
|---|---|
| **00-overview.md** (this file) | Everyone: background, scope, glossary, reading order |
| [01-feature-list.md](01-feature-list.md) | Product/planning view: every feature with ID, priority (P0/P1/P2) and scope |
| [02-architecture.md](02-architecture.md) | System design: workspace layout, core type drafts, codec boundary, dependencies, async model |
| [03-development-plan.md](03-development-plan.md) | Plan and tracking: milestones M0–M8, iterations, progress tables, risks |
| [04-data-model.md](04-data-model.md) | Module spec: identifier system, dataset, elements, charsets, validation (M1–M2) |
| [05-file-io.md](05-file-io.md) | Module spec: buffers, byte sources, parser, writer, DicomFile (M3) |
| [06-pixel-codec.md](06-pixel-codec.md) | Module spec & contract: PixelData facade, Codec trait, registry, transcoder (M4) — **the radicom-codecs interface contract** |
| [07-network.md](07-network.md) | Module spec: PDU formats, association negotiation, protocol engine, services, client/server (M5–M6) |
| [08-serialization-media.md](08-serialization-media.md) | Module spec: DICOM JSON/XML, DICOMDIR, structured reports, printing models + layout rendering, anonymizer (M7, P2 parts) |
| [09-testing.md](09-testing.md) | Quality: test data inventory, tolerance corpus, round-trip/interop/fuzz/benchmark strategy |
| [10-rendering.md](10-rendering.md) | Module spec: DicomImage, pipelines, LUT family, render options, pixel types, color conversion, image backends, geometry/localizers, MPR (M8, P2) |

Reading order suggestion: 00 → 01 → 02 → 03 for orientation; then the module specs (04–09) as each milestone enters development.

## 5. Glossary

| Term | Meaning |
|---|---|
| **AE (title)** | Application Entity title: up to 16 bytes identifying a DICOM network endpoint |
| **Attribute** | A tagged data element (tag + VR + value) |
| **Command set** | The DIMSE command dataset, always encoded Implicit VR Little Endian |
| **DICOMDIR** | A DICOM file of SOP class Media Storage Directory indexing a file set |
| **DIMSE** | DICOM Message Service Element: C-ECHO/C-STORE/C-FIND/C-GET/C-MOVE/N-* services |
| **Encapsulated** | Pixel data stored as a fragment sequence (compressed transfer syntaxes) |
| **Fragment** | One item of an encapsulated pixel data sequence; the first fragment conventionally holds the basic offset table |
| **Group length** | The `(GGGG,0000)` element giving the encoded length of its group; required in file meta, legacy in datasets |
| **Item / Item delimiter** | `FFFE,E000` markers delimiting sequence items; sequences may use undefined length with `FFFE,E0DD` |
| **PDU / PDV** | Protocol Data Unit / Presentation Data Value: the network wire units |
| **Preamble** | The 128-byte file prefix followed by `DICM` |
| **Presentation context** | An (abstract syntax, transfer syntax list) pair negotiated per association, identified by an odd byte ID |
| **Private creator** | The LO element at `(odd group, 00xx)` naming the owner of a private block |
| **SCU / SCP** | Service Class User (client) / Service Class Provider (server) |
| **SOP class / instance** | Service-Object Pair class (UID-typed service+object type) / a concrete instance thereof |
| **SQ** | Value Representation "sequence of items" |
| **Transfer syntax** | The UID defining byte encoding rules (VR explicitness, endianness, compression) |
| **VR** | Value Representation: the 34 data types of DICOM attributes (AE, PN, OW, ...) |
| **VM** | Value Multiplicity: how many values an attribute may hold ("1", "1-n", "2-2n") |

## 6. Status and How to Use These Documents

- The documentation suite is the deliverable of the planning phase; **no code has been written yet**.
- Feature IDs are stable identifiers: they are referenced by the module specs, the plan and the progress tables, and must not be renumbered.
- Each module spec (04–09) opens with a status line linking to the plan; update [03-development-plan.md](03-development-plan.md) §3 whenever a feature group changes state.
- Design changes discovered during implementation flow **back** into the module specs via pull request — the specs are living documents, not one-shot artifacts.
