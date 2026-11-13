# radicom

A pure Rust implementation of the DICOM (Digital Imaging and Communications in Medicine) standard, ported from the [fo-dicom](https://github.com/fo-dicom/fo-dicom) C# library.

[![Crates.io](https://img.shields.io/crates/v/radicom.svg)](https://crates.io/crates/radicom)
[![Docs.rs](https://docs.rs/radicom/badge.svg)](https://docs.rs/radicom)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

radicom covers the DICOM data model, file I/O, network communication (DIMSE), serialization, media files, structured reports, printing data models, anonymization, pixel data access, and the codec **interfaces** plus transcoder.

Compressed transfer syntaxes are supported through the companion **[radicom-codecs](https://github.com/cocosip/radicom-codecs)** crate: twelve codecs (RLE Lossless, JPEG, JPEG-LS, JPEG 2000, HTJ2K) registered with one `radicom_codecs::install()` call. That split mirrors the relationship between `FO-DICOM.Core` and `fo-dicom.Codecs`.

## Workspace Layout

| Crate | Contents |
|---|---|
| **`radicom`** | Core library: data model, file I/O, pixel data, rendering, printing, media (DICOMDIR), SR, anonymization, XML/JSON helpers, codec interfaces |
| **`radicom-network`** | Async (tokio) DICOM networking: PDU codec, association negotiation, DIMSE C-/N-services, SCU client, SCP server, TLS |
| **`radicom-json`** | DICOM JSON Model (PS3.18) and Native XML serialization |
| **`radicom-dict`** | Generated tag / UID / private dictionary tables (from the pinned fo-dicom dictionary XML) |

The codec implementations are a separate companion workspace, not a member of this one: see
[Image Codecs](#image-codecs).

## Features

### Core Capabilities

- **DICOM File I/O** — Part 10 file parsing and writing: explicit/implicit VR, both endiannesses, sequences, fragments, large-value streaming (`ReadLargeOnDemand` / `SkipLargeTags` / `ReadAll`), deflate
- **Transfer Syntax Support** — Explicit/Implicit VR, Big/Little Endian, plus a registry for application-defined syntaxes
- **Character Encoding** — Multi-byte and ISO 2022 escape-sequence handling (UTF-8, Latin, Chinese, Japanese, Korean, Greek, Arabic, Hebrew, Cyrillic, Thai)
- **Fragment Sequences** — Read and write encapsulated pixel data with offset tables; encapsulated output is `OB` by default (`PixelDataVrMode::Compatible` reproduces fo-dicom 5.x's legacy `OW`); compressed syntaxes are supplied by `radicom-codecs`
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

### Image Codecs

`radicom` ships the codec **contract** only — `Codec`, `CodecRegistry`, `CodecParams` and the
`Transcoder` — as a frozen boundary (see [docs/architecture.md](docs/architecture.md) §9.1).
The implementations live in the companion crate
[`radicom-codecs`](https://github.com/cocosip/radicom-codecs)
([crates.io](https://crates.io/crates/radicom-codecs)): RLE Lossless, JPEG (Process 1, Process 2/4,
Lossless 14, Lossless 14 SV1), JPEG-LS Lossless/Near-Lossless, JPEG 2000 Lossless/Lossy and HTJ2K
Lossless/Lossless RPCL/Lossy — twelve transfer syntaxes in total.

```rust,ignore
// Registers every codec in the process-wide registry, once at startup.
radicom_codecs::install();
```

Without that call only uncompressed-to-uncompressed transcoding works; a compressed source or
target fails with `CodecError::NotRegistered`.

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
| Anonymization | PS3.15 profiles, date shifting, UID remapping, recursive sequence handling, custom profiles | ✅ | `radicom::anonymizer` |
| JSON / XML | DICOM JSON Model (PS3.18), Native XML export | ✅ | `radicom-json` |
| DIMSE | C-ECHO/STORE/FIND/MOVE/GET, N-EVENT-REPORT/GET/SET/ACTION/CREATE/DELETE | ✅ | `radicom-network` |
| Network transports | tokio TCP, rustls TLS (PEM), max-PDU negotiation, role selection, user identity | ✅ (feature `tls`) | `radicom-network` |
| Observability | Structured tracing events and metrics hooks | ✅ | `radicom-network` |
| Image codecs | RLE Lossless, JPEG, JPEG-LS, JPEG 2000 and HTJ2K **implementations** | ✅ (12 transfer syntaxes) | companion crate [`radicom-codecs`](https://github.com/cocosip/radicom-codecs) |

Legend: ✅ implemented and tested. The codec **interfaces** (`Codec`, `CodecRegistry`, `CodecParams`, transcoder) are implemented in this workspace; the concrete codec implementations ship in `radicom-codecs`.


## Installation

```toml
[dependencies]
radicom = "0.2.0"
radicom-network = "0.2.0"   # only if you need DIMSE networking
radicom-json = "0.2.0"      # only if you need DICOM JSON / XML
tokio = { version = "1", features = ["macros", "rt-multi-thread"] } # networking examples
```

`radicom-codecs` is a separate crate, not a feature of `radicom`. For compressed images, add a
companion release whose `radicom` dependency accepts `0.2`, then call `radicom_codecs::install()`
once at startup (see [Image Codecs](#image-codecs)). The codec interfaces are unchanged, but
`radicom-codecs 0.1.2` depends on `radicom ^0.1.2`: combining it with `radicom 0.2` creates two core
versions with separate codec registries. It does not register codecs in the `0.2` core.

Optional features of `radicom`:

| Feature | Enables |
|---|---|
| `render` | The image rendering pipeline (`radicom::render`) |
| `anonymize` | Compatibility feature; `radicom::anonymizer` is available without it |

```toml
radicom = { version = "0.2.0", features = ["render", "anonymize"] }
```

Other workspace features:

| Crate | Feature | Enables |
|---|---|---|
| `radicom` | `render` | Rendering pipeline, image output helpers |
| `radicom` | `anonymize` | Compatibility feature; anonymization is always available |
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

For the console example, add `tracing-subscriber` with its `env-filter` feature:

```toml
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

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
    ..Default::default()
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

### Transcoding and Pixel Data VR

`Transcoder` and the `clone_with_syntax` convenience APIs move pixel data between transfer syntaxes
through the codec registry. Uncompressed-to-uncompressed transcoding works standalone; compressed
sources or targets resolve their codecs after `radicom_codecs::install()` has registered the
companion crate's twelve codecs (see [Image Codecs](#image-codecs)).

```rust
use radicom::io::file::DicomFile;
use radicom::model::transfer_syntax::syntaxes;
use radicom::pixel::PixelDataVrMode;

fn main() -> Result<(), radicom::Error> {
    let file = DicomFile::open("ct_image.dcm")?;

    // Same pixels, Explicit VR Little Endian encoding.
    let uncompressed =
        file.clone_with_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(), None)?;

    // Encapsulated output is OB at every bit depth (PS3.5 §8.2). `Compatible`
    // writes the legacy OW fragments of fo-dicom 5.x for byte-identical files.
    let legacy = file.clone_with_syntax_and_vr_mode(
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
        None,
        PixelDataVrMode::Compatible,
    )?;

    uncompressed.save("uncompressed.dcm")?;
    legacy.save("legacy_vr.dcm")?;
    Ok(())
}
```

Reading stays tolerant: an existing `OW` fragment sequence is kept and round-trips unchanged.
`Transcoder::with_pixel_data_read_mode(PixelDataVrMode::Standard)` opts into strict input validation,
while `Transcoder::with_strict_dicom_vr(bool)` is the go-dicom-style alias that changes only the
**write** mode (`true` = `Standard`, `false` = `Compatible`).

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
use radicom::{Dataset, Tag, Vr};

let mut ds = Dataset::new();
ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "12345");
```

`WriteOptions` controls sequence/item lengths and group-length emission:

```rust
use radicom::io::writer::WriteOptions;

let options = WriteOptions {
    explicit_length_sequences: true,      // defined instead of undefined length
    explicit_length_sequence_items: true,
    keep_group_lengths: false,            // recalculate rather than strip (GGGG,0000)
    ..Default::default()
};
```

Saved files identify the implementation as `RADICOM <version>` in `(0002,0013) Implementation Version
Name`; the value is derived from the package version at build time, so a release bump renames it
automatically. Association user information advertises the same name.

### Character Encoding

Character sets are handled automatically from `(0008,0005) Specific Character Set`, including ISO 2022 escape sequences and multi-byte encodings. Values come back decoded as UTF-8:

```rust
use radicom::{io::file::DicomFile, Tag};

let file = DicomFile::open("chinese_patient.dcm")?;
let name = file.dataset.get_str(Tag::PATIENT_NAME)?;
println!("Patient Name: {name}");
```

### Anonymization

Applies the PS3.15 basic profile by default; no feature flag is required.

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
use radicom::anonymizer::Anonymizer;

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
use radicom::media::{scan_directory, ScanEvent, ScanOptions};

fn main() -> Result<(), radicom::Error> {
    let options = ScanOptions {
        glob: Some("*.dcm".to_string()),
        ..Default::default()
    };
    let summary = scan_directory(
        "studies/",
        &options,
        &mut |event| {
            if let ScanEvent::File { path, .. } = event {
                println!("{}", path.display());
            }
        },
        |_path| Ok(()),
    )?;
    println!("{} files matched", summary.files_matched);
    Ok(())
}
```

> Scanning is metadata-only and stops before Pixel Data; results are delivered deterministically.
> Use `ScanControl` with `scan_directory_with_control` to cancel a running scan, or
> `DicomDirectoryBuilder` to feed the matches straight into a DICOMDIR.

## DICOM Networking

Requires `radicom-network`. The API is async (tokio).

### C-ECHO Client

```rust
use radicom::uids;
use radicom_network::client::DicomClient;

#[tokio::main]
async fn main() -> radicom_network::Result<()> {
    let mut client = DicomClient::new("MY-SCU", "ANY-SCP", "127.0.0.1:11112");
    let response = client.send_c_echo(uids::VERIFICATION).await?;
    println!("status {:#06X}", response.status);
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

    // STUDY level, Study Root information model; 0 allocates a Message ID.
    let request = CFindRequest::study_query(0)
        .with_tag(Tag::PATIENT_ID, Vr::Lo, "PAT001")   // PatientID (match key)
        .with_tag(Tag::PATIENT_NAME, Vr::Pn, "");     // PatientName (return key)

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
`worklist_query`. `worklist_query` builds the Modality Worklist shape — its keys live inside the
Scheduled Procedure Step Sequence, with no Query/Retrieve Level. For the Patient Root information model
use `CFindRequest::new(QueryRetrieveLevel::Study, QueryRoot::Patient, message_id)`.

High-level send APIs treat Message ID `0` as automatic allocation. Nonzero IDs must be unique while
outstanding; responses expose the actual ID in `message_id_being_responded_to`. The low-level wire
codec still preserves a literal zero ID.

For progressive results on `DicomClient`, use `send_requests` below. The lower-level `Association`
provides `send_c_find_stream`, `send_c_move_stream` and `send_c_get_stream` when the caller manages
presentation contexts itself.

To cancel a running FIND/GET/MOVE, obtain `client.cancellation_handle()` before sending and call
`handle.cancel(actual_message_id)` from another task or a mixed-response callback. For typed calls,
choose a nonzero ID when the cancelling task must know it before the first response. A mixed callback
receives the allocated `event.message_id`. `DicomClient::cancel` borrows the client and cannot be
called concurrently with its `send_*` future.

### Mixed DIMSE Requests

`send_requests` accepts concrete request objects converted into `DicomRequest`; it negotiates the
union of their presentation contexts and delivers pending and final responses through one callback:

```rust
use radicom::uids;
use radicom_network::{CEchoRequest, CFindRequest, DicomClient, DicomRequest};

#[tokio::main]
async fn main() -> radicom_network::Result<()> {
    let requests = vec![
        DicomRequest::from(CEchoRequest {
            message_id: 0,
            affected_sop_class_uid: uids::VERIFICATION.to_string(),
        }),
        DicomRequest::from(CFindRequest::study_query(0)),
    ];
    let mut client = DicomClient::new("MY-SCU", "QR-SCP", "127.0.0.1:11112");
    client.send_requests(requests, |event| {
        println!("request {}: ID {}, final={}, {:?}",
            event.request_index, event.message_id, event.is_final, event.response);
        Ok(())
    }).await?;
    client.release().await;
    Ok(())
}
```

The same collection accepts `CStoreRequest`, `CMoveRequest`, `CGetRequest` and `NServiceRequest`.
Use `DicomRequest::store(Arc<DicomFile>)` to send a file directly, or a `CStoreRequest` to preserve
explicit command fields such as the Move Originator. `DicomRequestQueue` / `send_queue` are optional
wrappers; `send_batch` remains a compatibility alias. The sender admits at most 64 simultaneous
operations, subject to the negotiated window. A C-GET drains earlier requests and completes before
later requests are admitted so reverse C-STORE can make progress.

### Receiving C-GET Instances

Register a reverse C-STORE handler and the storage SOP classes the peer will return. For example,
this client collects CT instances; include every expected storage class for a mixed study:

```rust
use std::sync::{Arc, Mutex};
use radicom::uids;
use radicom_network::{CGetRequest, DicomClient};

#[tokio::main]
async fn main() -> radicom_network::Result<()> {
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut client = DicomClient::new("MY-SCU", "QR-SCP", "127.0.0.1:11112")
        .collect_reverse_stores(received.clone())
        .with_reverse_store_classes(vec![uids::CT_IMAGE_STORAGE.to_string()]);
    let responses = client.send_c_get_registered(&CGetRequest::study("1.2.3", 0)).await?;
    println!("{} responses, {} instances", responses.len(), received.lock().unwrap().len());
    client.release().await;
    Ok(())
}
```

`send_c_get(&request, &storage_sop_classes)` accepts classes per call instead. In mixed collections,
use `DicomRequest::get(request, classes)` or register the classes once as above. C-MOVE sends instances
on a separate association to the destination AE; configure that AE's listening SCP at the remote peer.

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

Requires the `tls` feature. TLS material lives in the connection options:

```rust
use radicom_network::client::{DicomClient, DicomClientOptions};
use radicom_network::server::ClientOptions;
use radicom_network::tls::TlsOptions;

let client = DicomClient::new("MY-SCU", "SECURE-SCP", "pacs.example.com:2762")
    .with_options(DicomClientOptions {
        connection: ClientOptions {
            tls: Some(TlsOptions::client(Some("ca.pem"))),
            ..Default::default()
        },
        ..Default::default()
    });
```

### Observability

Register an event handler and/or metrics sink; records carry operational metadata only (no datasets, PDU payloads, pixel data or patient attributes):

```rust
use std::sync::Arc;
use radicom_network::DicomClient;

let client = DicomClient::new("MY-SCU", "ANY-SCP", "127.0.0.1:11112");
client.with_event_handler(Arc::new(|event| {
    println!("{event:?}");
}));
```

### Hosting

`radicom_network::hosting::HostedDicomServer` wraps `DicomServer` with a config, `health()` snapshot and graceful stop — for embedding in an application runtime.

## Structured Reports

```rust
use radicom::sr::{CodeItem, ContentItem, Relationship};

let concept = CodeItem::new("T-A0100", "SRT")
    .with_meaning("Chest")
    .with_version("1");

let item = ContentItem::text(concept, Relationship::Contains, "No abnormality detected");
```

## Printing

`radicom::print` provides dataset-backed Film Session, Film Box, Image Box and Presentation LUT models plus layout rendering (`render_to_image`), driven by the same render pipeline.

## Benchmarks

The workspace ships four dependency-free benchmark binaries (`harness = false`, `std::time::Instant`) so they run offline without `criterion`:

| Binary | Measures | Command |
|---|---|---|
| `crates/radicom/benches/core.rs` | `parse_dataset`, `write_dataset`, `get_frame`, `render_frame`, `open_800_elements`, `from_bytes_800` | `cargo bench -p radicom --all-features` |
| `crates/radicom/benches/workflows.rs` | charset encode/decode (UTF-8 + ISO 2022), deflate, uncompressed transcoding, validation, DICOMDIR build/save, directory scan, on-demand open, anonymize | `cargo bench -p radicom --all-features` |
| `crates/radicom-json/benches/json.rs` | `dataset_to_json`, `json_to_dataset`, JSON round-trip | `cargo bench -p radicom-json` |
| `crates/radicom-network/benches/throughput.rs` | loopback C-STORE throughput (`Memory` and `SpoolToTemp` sinks) | `cargo bench -p radicom-network` |

The `workflows.rs` anonymize row only runs with `--all-features`.

Compile-check all targets without running them:

```bash
cargo bench --workspace --no-run
```

Example output from the `radicom` core suite on a developer machine (absolute numbers are CPU- and file-dependent):

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

See [docs/development-guide.md](docs/development-guide.md) for the full test strategy, data inventory and tolerance corpus.

### Coverage

The all-feature Windows run measured production-source line coverage at **95.69%** (26,414 / 27,605
lines), using `rustc -C instrument-coverage` + `llvm-cov`. Third-party crates, generated dictionary
tables, `xtask`, test/bench/example targets, inline `#[cfg(test)]` modules and source files starting
with `#![cfg(test)]` are excluded. The breakdown is core **96.09%**, network **95.04%**, JSON **95.39%**.
Platform-specific paths produce different measurements in CI (`#[cfg(windows)]` lines only execute on
Windows); the production-line floor is 90%.

Run coverage locally with `llvm-tools-preview` and `cargo-llvm-cov`:

```bash
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked
cargo llvm-cov --workspace --all-features
```


## Regenerating the Dictionaries

Tag, UID and private dictionary tables are generated from the pinned fo-dicom dictionary XML under `crates/radicom-dict/dictionaries/`:

```bash
cargo run --package xtask -- gen-dict
```

`xtask` is the workspace's maintenance crate; CI invokes the same long form, and the repository ships no
`.cargo/config.toml`, so nothing relies on a local alias. Keep machine-specific cargo settings in your own
untracked `.cargo/` directory.

To move to a newer dictionary baseline, replace both XML files, run the generator, and commit the XML plus all regenerated outputs together.

## Relationship to fo-dicom

This project is a Rust port inspired by [fo-dicom](https://github.com/fo-dicom/fo-dicom). The design keeps fo-dicom's semantics (tolerance behaviours included) while using Rust idioms: traits and generics instead of dependency-injection containers and inheritance, `Result`/`Option` instead of exception tiers, and zero-copy buffer views for large values.

The mapping from fo-dicom to radicom — scope, intentional divergences and out-of-scope areas — is recorded in:

- [docs/development-guide.md](docs/development-guide.md) — scope, baseline, known limits, testing, interop and release process
- [docs/architecture.md](docs/architecture.md) — architecture and the intentional divergences (§14)

## Documentation

| Document | Content |
|---|---|
| [docs/architecture.md](docs/architecture.md) | **What the code must obey**: tag ordering, tolerance rules T1–T10, the frozen codec boundary, wire formats, divergences |
| [docs/development-guide.md](docs/development-guide.md) | Scope and baseline, known limits, test strategy, interop testing, code generation, CI and publishing |

## License

radicom's own code is licensed under the **MIT License** — see [LICENSE](LICENSE).

Third-party data: the DICOM dictionary XML files reused from fo-dicom (and the tables generated from them) are licensed under the **Microsoft Public License (MS-PL)** and retain the fo-dicom copyright notice and attribution — see [NOTICE](NOTICE).

## CI/CD

| Trigger | What runs |
|---|---|
| push / PR to `master` or `main` | `ci.yml`: clippy and tests with default **and** all features, docs and publishable-metadata checks on Linux + Windows, plus an MSRV (1.89) check. **No publishing.** |
| push of a `v*` tag | `release.yml`: the tag must equal every publishable crate's version; the release gates run on Ubuntu (`fmt --check`, clippy, tests, docs, gen-dict, 90% production coverage, package metadata, MSRV); then `radicom-dict` → `radicom` → `radicom-json` → `radicom-network` are published to crates.io in that order, and finally a GitHub release is created whose notes list the commits since the previous tag |

`dev` is intentionally not wired to CI. Publishing requires a `CARGO_REGISTRY_TOKEN` repository secret (a
crates.io API token with `publish-new` and `publish-update` scopes); releasing is done by pushing a tag:

```pwsh
git tag v0.2.0
git push origin v0.2.0
```

The version lives once in the root `Cargo.toml` (`[workspace.package] version`, mirrored by the internal
dependency pins in `[workspace.dependencies]`); every crate inherits it, so there is no per-crate version to
keep in sync. A failed tag run can be re-run as-is: crates whose version already reached crates.io are skipped
instead of retried.

Release notes are generated from the commit history, not written by hand: `release.yml` lists every non-merge
commit subject between the previous tag and this one, plus a `compare` link (the first release lists the whole
history and omits the link). To (re)generate the notes for a tag that was already published, run the `Release`
workflow manually (**Actions → Release → Run workflow**) with that tag — a manual run never publishes crates.

### Migrating from 0.1 to 0.2

`CommonExtended` now includes the SOP Class UID and `common_extended` is a `Vec` on both
`ClientOptions` and `UserInformation`. Replace `None` with `Vec::new()` and `Some(item)` with
`vec![item]`, supplying `sop_class_uid` on each item:

```rust
use radicom::uids;
use radicom_network::{CommonExtended, server::ClientOptions};

let options = ClientOptions {
    common_extended: vec![CommonExtended {
        sop_class_uid: uids::CT_IMAGE_STORAGE.to_string(),
        service_class_uid: uids::STORAGE.to_string(),
        related_general_sop_classes: Vec::new(),
    }],
    ..Default::default()
};
```

The default receive spool limit is now 4 GiB with a 1 MiB memory threshold. Configure
`SpoolConfig::max_message_length` / `memory_threshold` explicitly for larger instances and set
`ClientOptions::reverse_spool` for C-GET receives. Missing optional C-service response UIDs decode as
empty strings; absent terminal GET/MOVE counters decode as zero, while Pending counters remain required.
`CEchoResponse::success` takes `&CEchoRequest` to preserve its Message ID and SOP class.
The codec traits and transcoder interface remain unchanged; upgrade the companion's core dependency
to `0.2` and validate it before using compressed transfer syntaxes with this release.

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
