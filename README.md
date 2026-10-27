# radicom

A DICOM processing and encoding/decoding library for Rust, modeled on [fo-dicom](https://github.com/fo-dicom/fo-dicom) (Fellow Oak DICOM).

Repository: <https://github.com/cocosip/radicom>

radicom covers the DICOM data model, file I/O, network communication (DIMSE), serialization, media files, structured reports, printing data models, anonymization, pixel data access and the codec **interfaces** plus transcoder. The concrete image codec implementations (JPEG, JPEG-LS, JPEG 2000, HTJ2K, RLE native codecs) live in the separate **radicom-codecs** project — mirroring the relationship between `FO-DICOM.Core` and `fo-dicom.Codecs`.

## Documentation

| Document | Content |
|---|---|
| [docs/00-overview.md](docs/00-overview.md) | Project overview: goals, scope, baseline, glossary, documentation map |
| [docs/01-feature-list.md](docs/01-feature-list.md) | Feature list with IDs, priorities and scope (127 items; codec implementations explicitly out of scope) |
| [docs/02-architecture.md](docs/02-architecture.md) | Overall architecture: workspace layout, core type design, the radicom ↔ radicom-codecs boundary, dependencies, async model |
| [docs/03-development-plan.md](docs/03-development-plan.md) | Milestone plan (M0–M8), iteration breakdown and **progress tracking tables** |
| [docs/04-data-model.md](docs/04-data-model.md) | Module spec: identifiers, dataset, elements, charsets, validation (M1–M2) |
| [docs/05-file-io.md](docs/05-file-io.md) | Module spec: buffers, byte sources, parser (with tolerance rules), writer, DicomFile (M3) |
| [docs/06-pixel-codec.md](docs/06-pixel-codec.md) | Module spec and **interface contract**: PixelData facade, Codec trait, registry, transcoder (M4) — the radicom-codecs boundary |
| [docs/07-network.md](docs/07-network.md) | Module spec: PDU formats, association negotiation, protocol engine, DIMSE services, client/server (M5–M6) |
| [docs/08-serialization-media.md](docs/08-serialization-media.md) | Module spec: DICOM JSON/XML, DICOMDIR, structured reports, printing models + layout rendering, anonymizer (M7, P2 parts) |
| [docs/09-testing.md](docs/09-testing.md) | Test strategy: data inventory, tolerance corpus, round-trip/interop/fuzz/benchmarks, CI stages |
| [docs/10-rendering.md](docs/10-rendering.md) | Module spec: DicomImage, pipelines, LUTs, render options, pixel types, color conversion, backends, geometry, MPR (M8, P2) |

## Status

Milestones M0–M3 are complete (workspace, identifier system, dataset/elements, file IO). Reading and writing round-trip across explicit/implicit VR, both endiannesses, sequences, encapsulated pixel data and deflate; the parser covers the malformed-data tolerance corpus (T1–T10). Pixel data and the codec interface (M4) are next. See the progress tables in the development plan.

## License

TBD for radicom's own code. Note: the DICOM dictionary XML files reused from fo-dicom (and tables generated from them) are licensed under the **Microsoft Public License (MS-PL)** and must retain the fo-dicom copyright notice and attribution.
