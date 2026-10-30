# radicom

A pure Rust implementation of the DICOM (Digital Imaging and Communications in Medicine) standard, ported from the [fo-dicom](https://github.com/fo-dicom/fo-dicom) C# library.

[![Crates.io](https://img.shields.io/crates/v/radicom.svg)](https://crates.io/crates/radicom)
[![Docs.rs](https://docs.rs/radicom/badge.svg)](https://docs.rs/radicom)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

radicom covers the DICOM data model, file I/O, network communication (DIMSE), serialization, media files, structured reports, printing data models, anonymization, pixel data access, and the codec **interfaces** plus transcoder.

The concrete image codec implementations (JPEG, JPEG-LS, JPEG 2000, HTJ2K, RLE native codecs) live in the separate **[radicom-codecs](https://github.com/cocosip/radicom-codecs)** project — mirroring the relationship between `FO-DICOM.Core` and `fo-dicom.Codecs`.

## Workspace Layout

| Crate | Contents |
|---|---|
| **`radicom`** | Core library: data model, file I/O, pixel data, rendering, printing, media (DICOMDIR), SR, anonymization, XML/JSON helpers, codec interfaces |
| **`radicom-network`** | Async (tokio) DICOM networking: PDU codec, association negotiation, DIMSE C-/N-services, SCU client, SCP server, TLS |
| **`radicom-json`** | DICOM JSON Model (PS3.18) and Native XML serialization |
| **`radicom-dict`** | Generated tag / UID / private dictionary tables (from the pinned fo-dicom dictionary XML) |

## Features

### Core Capabilities

- **DICOM File I/O** — Part 10 file parsing and writing: explicit/implicit VR, both endiannesses, sequences, fragments, large-value streaming (`ReadLargeOnDemand` / `SkipLargeTags` / `ReadAll`), deflate
- **Transfer Syntax Support** — Explicit/Implicit VR, Big/Little Endian, plus a registry for application-defined syntaxes
- **Character Encoding** — Multi-byte and ISO 2022 escape-sequence handling (UTF-8, Latin, Chinese, Japanese, Korean, Greek, Arabic, Hebrew, Cyrillic, Thai)
- **Fragment Sequences** — Read and write encapsulated pixel data with offset tables; compressed syntaxes are supplied by `radicom-codecs`
- **Dataset Operations** — Rich typed access (`get` / `try_get` / `get_or` / `get_str`), masked-tag matching, walkers, copy
- **DICOMDIR Media** — Create, read, stitch and save media directories, with optional generated icons
- **File Scanning** — Cancellable metadata-only directory scanning with bounded concurrency
- **JSON / XML Serialization** — DICOM JSON Model (PS3.18) and Native XML export
- **Anonymization** — PS3.15 de-identification profiles (basic + configurable), date shifting, UID remapping, recursive sequence handling
- **Structured Reports** — Typed content items, hierarchical structure, code items, measured values, referenced SOP instances
- **Pixel Data & Rendering** — Frame access, photometric conversion, Modality/VOI/Palette LUTs, windowing, overlays, rendering pipeline, geometry/localizers, MPR, PNG/BMP output
- **DICOM Printing** — Film Session / Film Box / Image Box / Presentation LUT data models and basic print workflow
- **DICOM Networking** — DIMSE C-ECHO/STORE/FIND/MOVE/GET and N-EVENT-REPORT/GET/SET/ACTION/CREATE/DELETE; SCU client, SCP server framework, TLS, asynchronous operations, role selection, extended negotiation, user identity
- **Network Observability** — Vendor-neutral structured events and metrics hooks with metadata-only records

### Explicitly Out of Scope

Concrete codec implementations are **not** in this repository. `radicom` ships the codec **registry, traits and transcoder** only — a frozen boundary that [`radicom-codecs`](https://github.com/cocosip/radicom-codecs) implements. Mirroring `FO-DICOM.Core` ↔ `fo-dicom.Codecs`.

## Support Matrix

| Area | Capability | Status | Where |
|---|---|---|---|
| Data model | Tags, VRs, UIDs, VM, dictionaries, private creator block, masked tags, walkers, copy/compare | ✅ | `radicom::model` |
| File I/O | Part 10 read/write, explicit/implicit VR, little/big endian, sequences, fragments, deflate | ✅ | `radicom::io` |
| Large values | `ReadLargeOnDemand` file windows, `SkipLargeTags`, `ReadAll`, chunked writer | ✅ | `radicom::io`, `model::buffer` |
| Character sets | ISO 2022, UTF-8, Latin/CJK/Greek/Arabic/Hebrew/Cyrillic/Thai | ✅ | `radicom::encoding` |
| Validation | Per-VR validators, VM checks, dataset-level and scoped switches | ✅ | `radicom::validation` |
| Pixel data | Frame access, photometric conversion, Modality/VOI/Palette LUTs, overlays, MPR geometry | ✅ | `radicom::pixel`, `radicom::render` |
| Rendering | Frame rendering, windowing, scaling, PNG/BMP output | ✅ (feature `render`) | `radicom::render` |
| Printing | Film Session / Film Box / Image Box / Presentation LUT and layout rendering | ✅ | `radicom::print` |
| Media | DICOMDIR read/stitch/write, icons, file scanner | ✅ | `radicom::media` |
| Structured Reports | Content tree, 15 ValueTypes / 7 relationships, code items, measured values, referenced SOPs | ✅ | `radicom::sr` |
| Anonymization | PS3.15 profiles, date shifting, UID remapping, recursive sequence handling, custom profiles | ✅ (feature `anonymize`) | `radicom::anonymizer` |
| JSON / XML | DICOM JSON Model (PS3.18), Native XML export | ✅ | `radicom-json` |
| DIMSE | C-ECHO/STORE/FIND/MOVE/GET, N-EVENT-REPORT/GET/SET/ACTION/CREATE/DELETE | ✅ | `radicom-network` |
| Network transports | tokio TCP, rustls TLS (PEM), max-PDU negotiation, role selection, user identity | ✅ (feature `tls`) | `radicom-network` |
| Observability | Structured tracing events and metrics hooks | ✅ | `radicom-network` |
| Image codecs | JPEG / JPEG-LS / JPEG 2000 / HTJ2K / RLE **implementations** | ⛔ out of scope | separate [`radicom-codecs`](https://github.com/cocosip/radicom-codecs) project |

Legend: ✅ implemented and tested in this workspace; ⛔ deliberately out of scope. The codec **interfaces** (`Codec`, `CodecRegistry`, `CodecParams`, transcoder) are implemented here; only the concrete codec implementations live in `radicom-codecs`.


## Installation

```toml
[dependencies]
radicom = "0.1"
radicom-network = "0.1"   # only if you need DIMSE networking
radicom-json = "0.1"      # only if you need DICOM JSON / XML
```

Optional features of `radicom`:

| Feature | Enables |
|---|---|
| `render` | The image rendering pipeline (`radicom::render`) |
| `anonymize` | PS3.15 anonymization profiles (`radicom::anonymizer`, pulls in `regex`) |

```toml
radicom = { version = "0.1", features = ["render", "anonymize"] }
```

Other workspace features:

| Crate | Feature | Enables |
|---|---|---|
| `radicom` | `render` | Rendering pipeline, image output helpers |
| `radicom` | `anonymize` | PS3.15 anonymization profiles |
| `radicom` | `regex-rules` | Regex variants of match/transform rules (brings `regex`) |
| `radicom-network` | `tls` | rustls TLS transport (PEM certificates/keys) |
| `radicom-network` | `otel` | OpenTelemetry-style `tracing` metric convention helpers |

## Building from Source

Requires Rust **1.89+** (workspace MSRV):

```bash
git clone https://github.com/cocosip/radicom
cd radicom
cargo build --workspace --all-features
cargo test --workspace --all-features
```

## Logging

All library logging goes through the Rust [`tracing`](https://docs.rs/tracing) facade. The library does **not** install a global subscriber, so the application chooses the backend:

```rust
// Example: console logging in a binary crate.
tracing_subscriber::fmt()
    .with_env_filter("radicom::io=debug,radicom::network=info")
    .init();
```

`radicom` uses category targets (`radicom::io`, `radicom::network`, `radicom::codec`, `radicom::encoding`, `radicom::print`); any `tracing` subscriber or layer works, including `tracing-appender`, `tracing-opentelemetry` and `console-subscriber`. For applications built on `log`/`log4rs`, bridge via `tracing-log` rather than adding a second logging path inside the library.

## Quick Start

### Reading a DICOM File

```rust
use radicom::io::file::DicomFile;
use radicom::Tag;

fn main() -> Result<(), radicom::Error> {
    let file = DicomFile::open("example.dcm")?;
    let ds = &file.dataset;

    if let Some(name) = ds.try_get_str(Tag::PATIENT_NAME) {
        println!("Patient Name: {name}");
    }
    if let Some(id) = ds.try_get_str(Tag::PATIENT_ID) {
        println!("Patient ID: {id}");
    }
    if let Some(rows) = ds.try_get::<u16>(Tag::ROWS) {
        println!("Rows: {rows}");
    }
    println!("Total elements: {}", ds.len());
    Ok(())
}
```

> **Use the generated constants.** Every DICOM tag has a constant named after its
> PS3.6 keyword (`Tag::PATIENT_NAME`, `Tag::ROWS`, `Tag::SOP_CLASS_UID`, ...), and
> UIDs and transfer syntaxes have constants too (`uids::CT_IMAGE_STORAGE`,
> `syntaxes::EXPLICIT_VR_LITTLE_ENDIAN`). There is no reason to write `Tag::new(0x0010, 0x0010)`
> or a UID string by hand.

### Reading with Options

Control how large values (pixel data, big binary elements) are handled:

```rust
use radicom::io::file::{DicomFile, FileReadOption, ReadOptions};
use radicom::Tag;

// Skip large values entirely — metadata-only parsing.
let options = ReadOptions {
    read: FileReadOption::SkipLargeTags,
    large_object_size: 128 * 1024, // 128 KiB threshold
};
let file = DicomFile::open_with_options("large_image.dcm", &options)?;
assert!(file.dataset.item(&Tag::PIXEL_DATA).is_none());
```

`FileReadOption` variants: `ReadLargeOnDemand` (default; leaves large values as lazy file windows), `SkipLargeTags`, `ReadAll`.

### Accessing Pixel Data

```rust
use radicom::io::file::DicomFile;
use radicom::pixel::PixelData;

fn main() -> Result<(), radicom::Error> {
    let mut file = DicomFile::open("image.dcm")?;
    let pixels = PixelData::from_dataset(&mut file.dataset)?;

    println!("{}x{} bits={} samples={} frames={}",
        pixels.width(), pixels.height(),
        pixels.bits_allocated()?, pixels.samples_per_pixel(), pixels.number_of_frames());

    let frame0 = pixels.get_frame(0)?;
    println!("frame 0: {} bytes", frame0.size());
    Ok(())
}
```

### Rendering to an Image

Requires the `render` feature. `RawImage` exposes `width()`, `height()`, `pixel(x, y)` and can be written to PNG/BMP.

```rust
use radicom::io::file::DicomFile;
use radicom::render::render_frame;

fn main() -> Result<(), radicom::Error> {
    let file = DicomFile::open("ct_image.dcm")?;

    // Applies Modality LUT, VOI (window center/width), presentation LUT and color conversion.
    let image = render_frame(&file.dataset, 0)?;
    image.save("output.png")?;             // or to_png() / to_bmp()
    println!("rendered {}x{}", image.width(), image.height());
    Ok(())
}
```

### Writing a DICOM File

```rust
use radicom::io::file::DicomFile;
use radicom::io::writer::WriteOptions;
use radicom::model::dataset::Dataset;
use radicom::model::vr::Vr;
use radicom::{uids, Tag};

fn main() -> Result<(), radicom::Error> {
    let mut ds = Dataset::new();
    // The VR is inferred from the dictionary — no VR argument needed.
    ds.insert_value(Tag::SOP_CLASS_UID, uids::CT_IMAGE_STORAGE);
    ds.insert_value(Tag::SOP_INSTANCE_UID, "1.2.826.0.1.3680043.10.1142.1");
    ds.insert_value(Tag::PATIENT_NAME, "Doe^John");
    ds.insert_value(Tag::PATIENT_ID, "12345");
    ds.insert_value(Tag::MODALITY, "CT");
    ds.insert_value(Tag::ROWS, 128u16);
    ds.insert_value(Tag::COLUMNS, 128u16);

    let file = DicomFile::from_dataset(ds);
    file.save("generated.dcm")?;

    // or serialize to memory
    let bytes = file.to_bytes(&WriteOptions::default())?;
    println!("{} bytes", bytes.len());
    Ok(())
}
```

`insert_value` takes the VR from the dictionary, so `Tag::ROWS` becomes `US` and
`Tag::PATIENT_NAME` becomes `PN` automatically. Use `insert_str` only when you
need to force a VR explicitly:

```rust
ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "12345");
```

`WriteOptions` controls sequence/item lengths and group-length emission:

```rust
let options = WriteOptions {
    explicit_length_sequences: true,      // defined instead of undefined length
    explicit_length_sequence_items: true,
    keep_group_lengths: false,            // recalculate rather than strip (GGGG,0000)
};
```

### Character Encoding

Character sets are handled automatically from `(0008,0005) Specific Character Set`, including ISO 2022 escape sequences and multi-byte encodings. Values come back decoded as UTF-8:

```rust
let file = DicomFile::open("chinese_patient.dcm")?;
let name = file.dataset.get_str(Tag::PATIENT_NAME)?;
println!("Patient Name: {name}");
```

### Anonymization

Requires the `anonymize` feature. Applies the PS3.15 basic profile by default.

```rust
use radicom::anonymizer::Anonymizer;
use radicom::io::file::DicomFile;

fn main() -> Result<(), radicom::Error> {
    let file = DicomFile::open("study.dcm")?;

    let anon = Anonymizer::basic();
    let clean = anon.anonymize_file(&file)?;
    clean.save("study_anonymized.dcm")?;
    Ok(())
}
```

Customize the per-tag outcome (replace patient name/ID) with the builder methods:

```rust
let anon = Anonymizer::basic()
    .with_patient("ANON^PATIENT", "ANON-0001");
```

### JSON Serialization

```rust
use radicom::io::file::DicomFile;
use radicom_json::{dataset_to_json, json_to_dataset, JsonOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let file = DicomFile::open("example.dcm")?;

    let json = dataset_to_json(&file.dataset, &JsonOptions::default())?;
    println!("{json}");

    let restored = json_to_dataset(&json, &JsonOptions::default())?;
    assert_eq!(restored.len(), file.dataset.len());
    Ok(())
}
```

### DICOMDIR

```rust
use radicom::io::file::DicomFile;
use radicom::media::{BasicIconGenerator, DicomDirectory};

fn main() -> Result<(), radicom::Error> {
    let mut dir = DicomDirectory::new("FILESET1");

    let file = DicomFile::open("instance.dcm")?;
    // without an icon
    dir.add_file(&file, "IM0001")?;
    // with a generated 8-bit grayscale icon
    dir.add_file_with_icon(&file, "IM0002", &BasicIconGenerator::default())?;

    dir.save("DICOMDIR")?;
    Ok(())
}
```

### File Scanning

```rust
use radicom::media::scanner::{DicomFileScanner, ScannerOptions};

let scanner = DicomFileScanner::new(ScannerOptions::default());
for result in scanner.scan("studies/") {
    match result {
        Ok(entry) => println!("{}", entry.path.display()),
        Err(e) => eprintln!("unreadable: {e}"),
    }
}
```

> Scanning is metadata-only and stops before Pixel Data; results are delivered deterministically.

## DICOM Networking

Requires `radicom-network`. The API is async (tokio).

### C-ECHO Client

```rust
use radicom::uids;
use radicom_network::client::DicomClient;

#[tokio::main]
async fn main() -> radicom_network::Result<()> {
    let mut client = DicomClient::new("MY-SCU", "ANY-SCP", "127.0.0.1:11112");
    client.send_c_echo(uids::VERIFICATION).await?;
    println!("C-ECHO succeeded");
    client.release().await;
    Ok(())
}
```

### C-STORE Client

```rust
use radicom::io::file::DicomFile;
use radicom_network::client::DicomClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let file = DicomFile::open("scan.dcm")?;

    let mut client = DicomClient::new("MY-SCU", "STORE-SCP", "127.0.0.1:11112");
    let response = client.send_c_store(&file).await?;
    println!("status {:#06X}", response.status);
    client.release().await;
    Ok(())
}
```

### C-FIND / C-MOVE / C-GET

```rust
use radicom::model::vr::Vr;
use radicom::Tag;
use radicom_network::cfind::CFindRequest;
use radicom_network::client::DicomClient;

#[tokio::main]
async fn main() -> radicom_network::Result<()> {
    let mut client = DicomClient::new("MY-SCU", "QR-SCP", "127.0.0.1:11112");

    // STUDY level, Study Root information model (the default root), message id 1.
    let request = CFindRequest::study_query(1)
        .with_key(0x0010, 0x0020, Vr::Lo, "PAT001")   // PatientID (match key)
        .with_key(0x0010, 0x0010, Vr::Pn, "");        // PatientName (return key)

    let responses = client.send_c_find(&request).await?;
    for r in responses {
        if let Some(ds) = &r.identifier {
            println!("{:?}", ds.try_get_str(Tag::PATIENT_NAME));
        }
    }
    client.release().await;
    Ok(())
}
```

`CFindRequest` also provides `patient_query`, `series_query`, `image_query` and
`worklist_query`. For the Patient Root information model use
`CFindRequest::new(QueryRetrieveLevel::Study, QueryRoot::Patient, message_id)`.

For C-GET, received instances arrive as reverse C-STORE sub-operations; register a handler with `with_reverse_store_handler` / `collect_reverse_stores`. For progressive results use `send_c_find_stream` / `send_c_move_stream` / `send_c_get_stream`, and `cancel` to send C-CANCEL.

### SCP Server

Implement `ServiceProvider` and run `DicomServer`:

```rust
use std::sync::Arc;
use radicom::uids;
use radicom_network::server::{DicomServer, ServerOptions};
use radicom_network::services::{CEchoRequest, CEchoResponse, ServiceProvider};

struct MyProvider;

#[async_trait::async_trait]
impl ServiceProvider for MyProvider {
    fn supported_sop_classes(&self) -> Vec<String> {
        vec![uids::VERIFICATION.to_string()] // Verification
    }

    async fn on_c_echo(&self, request: &CEchoRequest) -> radicom_network::Result<CEchoResponse> {
        Ok(CEchoResponse::success(request))
    }
}

#[tokio::main]
async fn main() -> radicom_network::Result<()> {
    let server = DicomServer::bind("0.0.0.0:11112", Arc::new(MyProvider), ServerOptions::default()).await?;
    println!("listening on {}", server.local_addr());
    server.run().await
}
```

`ServiceProvider` is an `async_trait`, so the impl needs `#[async_trait::async_trait]` (add
`async-trait` to your dependencies). Every operation has a default that refuses, so a provider opts in
only to what it serves: `on_c_echo`, `on_c_store`, `on_c_find`, `on_c_move`, `on_c_get`, plus the
N-service handlers. `CEchoResponse::success` takes the request so it can echo the message ID and SOP
class back.

### TLS

```rust
use radicom_network::tls::TlsOptions;

let client = DicomClient::new("MY-SCU", "SECURE-SCP", "pacs.example.com:2762")
    .with_options(DicomClientOptions {
        tls: Some(TlsOptions::client(...)),
        ..Default::default()
    });
```

### Observability

Register an event handler and/or metrics sink; records carry operational metadata only (no datasets, PDU payloads, pixel data or patient attributes):

```rust
client.with_event_handler(Arc::new(|event| {
    println!("{event:?}");
}));
```

### Hosting

`radicom_network::hosting::HostedDicomServer` wraps `DicomServer` with a config, `health()` snapshot and graceful stop — for embedding in an application runtime.

## Structured Reports

```rust
use radicom::sr::code_item::CodeItem;
use radicom::sr::content_item::{ContentItem, RelationshipType};

let code = CodeItem::new("T-A0100", "SRT")
    .with_meaning("Chest")
    .with_version("1");

let item = ContentItem::text("Finding", "No abnormality detected")
    .with_concept_name(code);
```

## Printing

`radicom::print` provides dataset-backed Film Session, Film Box, Image Box and Presentation LUT models plus layout rendering (`render_to_image`), driven by the same render pipeline.

## Benchmarks

The workspace ships three dependency-free benchmark binaries (`harness = false`, `std::time::Instant`) so they run offline without `criterion`:

| Binary | Measures | Command |
|---|---|---|
| `crates/radicom/benches/core.rs` | `parse_dataset`, `write_dataset`, `get_frame`, `render_frame`, `open_800_elements`, `from_bytes_800` | `cargo bench -p radicom --all-features` |
| `crates/radicom-json/benches/json.rs` | `dataset_to_json`, `json_to_dataset`, JSON round-trip | `cargo bench -p radicom-json` |
| `crates/radicom-network/benches/throughput.rs` | loopback C-STORE throughput (`Memory` and `SpoolToTemp` sinks) | `cargo bench -p radicom-network` |

Compile-check all targets without running them:

```bash
cargo bench --workspace --no-run
```

Example output from `cargo bench -p radicom --all-features` on a developer machine (absolute numbers are CPU- and file-dependent):

| Benchmark | Result |
|---|---:|
| `parse_dataset` | 89,901 ns/op |
| `write_dataset` | 33,194 ns/op |
| `get_frame` | 851 ns/op |
| `render_frame` | 1,766,926 ns/op |
| `open_800_elements` | 283,438 ns/op |
| `from_bytes_800` | 221,211 ns/op |

Large values are designed to stay off-heap: `ReadLargeOnDemand` keeps them as file-backed windows and the writer streams in chunks, so parsing metadata for a large file does not require loading its pixel data.


## Testing

```bash
# Everything
cargo test --workspace --all-features

# A single crate
cargo test -p radicom --all-features
cargo test -p radicom-network --all-features

# Lints (CI runs with -D warnings)
cargo clippy --workspace --all-features --all-targets
```

Corpus and interop tests:

```bash
# Round-trip a corpus of real DICOM files (tests-data/)
cargo test -p radicom --test corpus --all-features

# External peer interop (skipped unless RADICOM_PEER is set)
$env:RADICOM_PEER = "127.0.0.1:11112"
cargo test -p radicom-network --test interop --all-features
```

See [docs/02-development-guide.md](docs/02-development-guide.md) for the full test strategy, data inventory and tolerance corpus.

### Coverage

Current measured production-source line coverage is **90.03%** (14,554 / 16,165 lines), using `rustc -C instrument-coverage` + `llvm-cov`, excluding third-party crates, generated dictionary tables, `xtask`, integration-test sources and `#[cfg(test)]` modules. The breakdown is approximately: core 90.19%, network 91.30%, JSON 76.76%.

Run coverage locally with `llvm-tools-preview` and `cargo-llvm-cov`:

```bash
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked
cargo llvm-cov --workspace --all-features
```


## Regenerating the Dictionaries

Tag, UID and private dictionary tables are generated from the pinned fo-dicom dictionary XML under `crates/radicom-dict/dictionaries/`:

```bash
cargo xtask gen-dict
```

`cargo xtask` is a repository alias defined in `.cargo/config.toml`
(`xtask = "run -p xtask --"`), so it works on a fresh checkout; the long form
`cargo run -p xtask -- gen-dict` is equivalent. Machine-specific cargo settings
still belong in your own `.cargo/config.toml`, which is gitignored apart from
that alias file.

To move to a newer dictionary baseline, replace both XML files, run the generator, and commit the XML plus all regenerated outputs together.

## Relationship to fo-dicom

This project is a Rust port inspired by [fo-dicom](https://github.com/fo-dicom/fo-dicom). The design keeps fo-dicom's semantics (tolerance behaviours included) while using Rust idioms: traits and generics instead of dependency-injection containers and inheritance, `Result`/`Option` instead of exception tiers, and zero-copy buffer views for large values.

The mapping from fo-dicom to radicom — feature parity, intentional divergences and out-of-scope areas — is recorded in:

- [docs/02-development-guide.md](docs/02-development-guide.md) — feature-by-feature mapping with IDs and priorities
- [docs/01-architecture.md](docs/01-architecture.md) — architecture and divergences D1–D12
- [docs/03-fo-dicom-alignment.md](docs/03-fo-dicom-alignment.md) — module-level alignment status

## Documentation

| Document | Content |
|---|---|
| [docs/README.md](docs/README.md) | Documentation index, project background, glossary |
| [docs/01-architecture.md](docs/01-architecture.md) | **What the code must obey**: Tag ordering, tolerance rules T1–T10, the frozen codec boundary, wire formats, known deviations |
| [docs/02-development-guide.md](docs/02-development-guide.md) | Feature list with IDs and priorities, milestone status, what is outstanding, test strategy and CI |
| [docs/03-fo-dicom-alignment.md](docs/03-fo-dicom-alignment.md) | Module-by-module fo-dicom parity verdict with code evidence |
| [docs/04-interop.md](docs/04-interop.md) | Interop testing against real peers |

## License

radicom's own code is licensed under the **MIT License** — see [LICENSE](LICENSE).

Third-party data: the DICOM dictionary XML files reused from fo-dicom (and the tables generated from them) are licensed under the **Microsoft Public License (MS-PL)** and retain the fo-dicom copyright notice and attribution — see [NOTICE](NOTICE).

## CI/CD

| Trigger | What runs |
|---|---|
| push / PR to `master` or `main` | `ci.yml`: clippy, tests, docs and publishable-metadata checks on Linux + Windows, plus an MSRV (1.89) check. **No publishing.** |
| push of a `v*` tag | `release.yml`: verifies the tag matches the crate version, re-runs tests, then publishes `radicom-dict` → `radicom` → `radicom-json` → `radicom-network` to crates.io |

`dev` is intentionally not wired to CI. Publishing requires a `CARGO_REGISTRY_TOKEN` repository secret; releasing is done by pushing a tag:

```pwsh
git tag v0.1.0
git push origin v0.1.0
```

## DICOM Resources

- [DICOM Standard](https://www.dicomstandard.org/)
- [fo-dicom](https://github.com/fo-dicom/fo-dicom) — the C# reference implementation this project is modeled on
- [go-dicom](https://github.com/cocosip/go-dicom) — the sibling Go port
- [DICOM Library](https://dicomlibrary.com/) — free DICOM samples for testing

## Contributing

Contributions are welcome. Please:

1. Follow Rust idioms and keep the public API documented.
2. Add tests for new functionality — ideally against a real file in `tests-data/`, not only a hand-built dataset.
3. Run `cargo test --workspace --all-features` and `cargo clippy --workspace --all-features --all-targets` before opening a PR.

## Acknowledgments

This project is heavily inspired by and based on [fo-dicom](https://github.com/fo-dicom/fo-dicom), an excellent DICOM library for .NET. Special thanks to the fo-dicom contributors for their comprehensive implementation of the DICOM standard.
