# Development Guide

> Testing, interop, maintenance and release. Design invariants live in
> [01-architecture.md](01-architecture.md); usage examples are in the root [README.md](../README.md).
> This document describes the current state only — history lives in `git log`.

## 1. Scope and Baseline

**In scope (`radicom`)**: DICOM data model, file reading/writing, network communication (DIMSE),
serialization, media files, structured reports, printing data model, anonymization, pixel data access
facade, codec **interfaces** and transcoder, rendering pipeline.

**Out of scope (`radicom-codecs`, a separate project)**: the concrete **implementations** of JPEG /
JPEG-LS / JPEG 2000 / HTJ2K / RLE codecs. `radicom` defines only the interfaces, parameter system,
registration mechanism and transcode flow. `radicom-codecs` mirrors `fo-dicom.Codecs` and injects its
implementations through the registration point.

| Item | Value |
|---|---|
| Reference implementation | fo-dicom (Fellow Oak DICOM), v5.2.x stable / 6.0.0-alpha dev source tree |
| DICOM standard edition tracked | 2025d (per the fo-dicom ChangeLog) |
| Bundled dictionary version | 2026b (5347 public tags, 4678 private tags across 235 creators) |
| Primary standard parts | PS3.3 (information model), PS3.5 (data structures/encoding), PS3.6 (data dictionary), PS3.7 (message exchange), PS3.8 (network communication), PS3.10 (media storage), PS3.15 (security profiles), PS3.18 (web services / JSON) |

**fo-dicom components with no direct port** (the capability is covered in a Rust-native shape; only the
.NET-specific packaging is not carried over):

| fo-dicom component | radicom equivalent |
|---|---|
| `FO-DICOM.AspNetCore` (IHostedService wrapper) | config-driven hosting helper on `radicom-network` |
| `FO-DICOM.Instrumentation` | `tracing`-convention metrics emission |
| `FO-DICOM.Json` (Newtonsoft backend) | serde. A second backend is redundant — divergence D11 |
| `FO-DICOM.Dump` (WPF GUI) | `radicom-dump` CLI plus the library dump formatter |
| WinForms / WPF / SkiaSharp / NetStandard image backends | one `RawImage` plus an optional backend — divergence D7 |
| GDI+ print rendering extensions | cross-platform FilmBox layout rendering |
| Managed codecs in Core (RLE, JPEG Lossless) | zero built-in codecs — divergence D10 |
| Legacy client plumbing (`DicomClientConnection`, state shims) | replaced by the async expert layer |

## 2. Known Limits

Deliberate, tested ceilings. Change one only together with the tests that describe it.

- The stateless compressed renderer decodes the full dataset once per render; there is no decoded-frame
  cache.
- Client batching covers C-ECHO, C-STORE and C-FIND (at most `min(invoked, 64)` outstanding); C-GET,
  C-MOVE and N-ACTION stay sequential. The SCP dispatches C-ECHO/C-STORE/C-FIND/N-services (except
  N-ACTION) to tasks but serialises each response through one writer, bounded by the AC window.
- A received C-STORE spills its PDVs to a temporary file (64 GiB bound) before parsing, but the parse
  still materialises the dataset once (the core exposes only byte-slice `parse_dataset_bytes`), and the
  SCU reverse-store (C-GET) path still buffers. Send-side streaming does not imply a file-backed parse.
- The read-side PDU cap is 256 KiB and applies to A-ASSOCIATE too. PS3.8's Max-PDU-Length formally
  governs only P-DATA-TF; the cap is a deliberate allocation-safety deviation.
- An external private sequence written with a *defined* length under Implicit VR still needs a private
  dictionary to recover its structure (its bytes are preserved).
- CP-246 (`explicit_vr = false`) is applied to the Big-Endian UN-undefined-length-SQ case only; applying
  it to Little Endian would break the `t3_un_undefined_length_parsed_as_sq` behaviour and real LE files.
- Concrete codecs, external peers, print/media hardware and non-Windows behaviour are not established by
  local tests.

## 3. Testing

### 3.1 Principles

1. **The tolerance corpus is a first-class deliverable**: every lenient behaviour (T1–T10, DICOMDIR
   nearest offset, JSON tolerance) has a named test referencing the originating issue or standard clause.
2. **Round-trips are the backbone**: read → write → re-read must be byte- or semantics-equal across the
   syntax matrix.
3. **Interop, not just self-consistency**: network paths are tested against external peers.
4. **No-panic guarantee**: parse and network decode paths are fuzzed; all failures are typed errors.
5. **A test must be able to fail.** If reverting the fix leaves the test green, it is not covering the
   defect. Verification for both directions of a bug fix is part of the work.

### 3.2 Organization

```
crates/radicom/src/**/tests.rs   # unit tests beside the code
crates/radicom/tests/            # integration: corpus, goldens, fuzz
crates/radicom-network/tests/    # loopback, goldens, fuzz, interop
crates/radicom-json/tests/       # JSON goldens
tests-data/                      # shared fixtures with a provenance README
```

Naming: `<area>_<behavior>__<case>`, with the rule ID suffix for tolerance tests (e.g.
`reader_un_vr_sq_recovery__T3`).

### 3.3 Test Data

A 23-sample corpus (22 `.dcm` files plus a `DICOMDIR`) from fo-dicom's test data lives in `tests-data/`
under the fo-dicom **MS-PL** license with a provenance note. Categories covered: syntax matrix
(explicit/implicit LE, explicit BE, deflated), encapsulated (RLE plus a JPEG baseline), malformed corpus
(one per T1–T10 row, synthesized where no fixture exists), multi-frame, charsets (CJK ISO 2022, GBK,
GB18030, UTF-8), structures (nested SQ, fragments, private tags), media (DICOMDIR), SR, meta variants (no
preamble / no meta / ACR-NEMA), and golden PDU byte captures.

To expand the corpus, drop fo-dicom-generated `.dcm` files into the matching `tests-data/<category>/`
directory and register them in the `CORPUS` table in `crates/radicom/tests/corpus.rs`.

### 3.4 Round-Trips and Goldens

- **Byte-level round-trip**: parse → write with identical options → byte-equal.
- **Semantic round-trip**: where byte equality is impossible (group length removal, padding), compare via
  dataset deep equality.
- **Goldens**: JSON output, DICOMDIR offsets, dump strings, PDU byte streams — generated once, reviewed,
  then enforced. Regenerate with `cargo run --package xtask -- regen-goldens`; CI fails on drift.

### 3.5 Network Testing

- **Loopback**: SCU↔SCP integration for echo/store/find/get/move, lifecycle, retry/linger/breaker with a
  scripted mock peer.
- **Reference interop** (optional runner): against DCMTK `storescp`/`storescu`/`echoscu` and a fo-dicom
  host. Gated on `RADICOM_PEER`; otherwise the tests self-skip.
- **Fuzz**: mutated PDU streams must produce typed errors without panics or unbounded reads.

### 3.6 Codec Testing

Only one configuration exists today: the empty registry plus the test-only mock codecs in
`pixel/codec.rs` and `pixel/transcoder.rs`, which validate the facade, the transcoder decision table, the
missing-codec error paths and the boundary contract itself.

A second configuration — end-to-end transcoding against the real `radicom-codecs` codecs — is added as an
optional dev-dependency once that project exists. Nothing in this workspace references it today, so this
repository never hard-depends on it.

### 3.7 Fuzzing

Pure `std`, deterministic, no `cargo-fuzz` needed: `crates/radicom/tests/fuzz.rs` sweeps arbitrary bytes,
byte-mutated valid files and a bogus ~4 GiB declared length through the parser;
`crates/radicom-network/tests/fuzz.rs` sweeps arbitrary PDU streams through `pdu::decode`. Everything must
return a typed `Result` without panicking. Memory guarantees: on-demand windows never load whole values,
and C-STORE receive is bounded by spooling.

### 3.8 Benchmarks

`criterion` is not vendored in the offline cache, so the suites are dependency-free `harness = false`
binaries using `std::time::Instant`:

| Binary | Measures | Run with |
|---|---|---|
| `crates/radicom/benches/core.rs` | parse_dataset, write_dataset, get_frame, render_frame, open() on-demand vs from_bytes for 800 elements | `cargo bench -p radicom --all-features` |
| `crates/radicom/benches/workflows.rs` | charset encode/decode (UTF-8 + ISO 2022), deflate, uncompressed transcoding, validation, DICOMDIR build/save, directory scan, on-demand open, anonymize | `cargo bench -p radicom --all-features` |
| `crates/radicom-json/benches/json.rs` | dataset_to_json, json_to_dataset, json_roundtrip | `cargo bench -p radicom-json` |
| `crates/radicom-network/benches/throughput.rs` | loopback C-STORE MiB/s (memory and spool sinks) | `cargo bench -p radicom-network` |

`workflows.rs` is feature-gated: the anonymize row only runs with `--all-features`. `core.rs` benchmarks
`from_bytes`; the file `open()` path is measured separately because on-demand windows make it a
different code path.

### 3.9 Coverage

Production-line coverage has a **95% floor**. `cargo run --package xtask -- coverage` runs
`cargo llvm-cov` and keeps only production lines: test/bench/example targets, `xtask` and inline
`#[cfg(test)]` modules are excluded. CI runs the same command and publishes the measured numbers.

## 4. Interop Testing Against Real Peers

The loopback regressions above exercise real local sockets, so a plain `cargo test` verifies radicom
against itself. Cross-implementation acceptance needs an external peer: a fo-dicom C-STORE SCP or Q/R
SCP, DCMTK, dcm4che, or a production PACS.

### 4.1 Offline Self-Test (runs by default)

The `self_interop_*` cases in `crates/radicom-network/tests/interop.rs` need no peer: they start a
`DicomServer` on `127.0.0.1` and drive real TCP with a `DicomClient`, verifying C-ECHO and a **real file
C-STORE over TCP → SCP → read back** with semantic equality (compared element by element after
normalising group lengths; the transfer syntax is preserved). `cargo test --workspace --all-features`
runs them.

### 4.2 Interop Tool: the `peer_interop` Example

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

### 4.3 Gated Interop Tests (optional in CI)

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

### 4.4 Acceptance Criteria

| Service | Peer | Acceptance |
|---|---|---|
| C-ECHO | Verification SCP | status 0x0000 (a C-STORE-only SCP may not support it — that is normal) |
| C-STORE | C-STORE SCP | status 0x0000; the peer's dataset is semantically equal to the source |
| C-FIND | Q/R SCP | correct match count and return keys (PatientName / StudyInstanceUID, ...) |
| C-MOVE | Q/R SCP + destination AE | correct completed sub-operation count; instances land at the destination |
| C-GET | Q/R SCP | correct completed reverse-C-STORE count; the reverse-store handler receives the instances |

> C-MOVE and C-GET need a destination-AE / reverse-store endpoint. radicom can play that role itself
> (`collect_reverse_stores` plus `DicomServer`), which the loopback suite already covers.

## 5. Code Generation and Maintenance

Tag, UID and private dictionary tables are generated from the pinned fo-dicom dictionary XML under
`crates/radicom-dict/dictionaries/`; never edit generated files manually.

```bash
cargo run --package xtask -- gen-dict          # regenerate
cargo run --package xtask -- gen-dict --check  # verify (CI gate)
```

To move to a newer dictionary baseline, replace both XML files, run the generator, and commit the XML plus
all regenerated outputs together. `cargo run --package xtask -- regen-goldens` rebuilds the golden
baselines and is run only after reviewing an intentional change. The repository ships no
`.cargo/config.toml`, so nothing relies on a local alias; keep machine-specific cargo settings in your own
untracked `.cargo/` directory.

## 6. CI and Releasing

### 6.1 CI stages

| Stage | Trigger | Content |
|---|---|---|
| clippy | push to `master`/`main`, pull request, `workflow_dispatch` | clippy `-D warnings` |
| test | push to `master`/`main`, pull request, `workflow_dispatch` | unit + integration, default and all-features |
| msrv | push to `master`/`main`, pull request, `workflow_dispatch` | MSRV (1.89) build |
| docs | push to `master`/`main`, pull request, `workflow_dispatch` | rustdoc build |
| package | push to `master`/`main`, pull request, `workflow_dispatch` | `cargo package --list` metadata/license checks |
| fuzz | local/manual | both fuzz targets |
| interop | local / self-hosted | DCMTK / fo-dicom matrix |
| bench | local/manual | `cargo bench` against a baseline |
| coverage | push to `master`/`main`, pull request, `workflow_dispatch` | `cargo run --package xtask -- coverage`, 95% production-line floor |
| release | push of a `v*` tag | `release.yml`: first `verify-tag` (the tag must equal every publishable crate's version, and the publishable set must equal the list `publish` pushes); then the same gates as `ci.yml` — `fmt --check`, clippy and tests with default **and** all features, `cargo doc` with `-D warnings`, `gen-dict --check`, `cargo package --list` proving `LICENSE` + `README.md` ship in every crate — plus the MSRV (1.89) check; only then are `radicom-dict` → `radicom` → `radicom-json` → `radicom-network` published to crates.io, in that dependency order, and a GitHub release is created with generated notes. Versions already on crates.io are skipped, so a failed tag run can be re-run from the same tag. A manual (`workflow_dispatch`) run with a tag input regenerates the release notes only and never publishes. |

### 6.2 Versioning and publishing

- The version lives once, in the root `Cargo.toml` under `[workspace.package]`. Every member inherits it, and
  internal dependencies are declared once under `[workspace.dependencies]` with that same version. Cargo refuses
  to build when a pin disagrees with the member version, so a half-finished bump fails on the next build instead
  of failing halfway through a publish.
- Bumping a release edits two blocks in the root manifest — `[workspace.package] version` and the four internal
  pins under `[workspace.dependencies]` — followed by the normal checks, a commit, and a `vX.Y.Z` tag. Keep the
  tag equal to the version: `release.yml` rejects any other tag.
- A bump also renames the peer-visible Implementation Version Name (`RADICOM <workspace version>`, currently
  `RADICOM 0.1.2`): file meta (`radicom`) and association user information (`radicom-network`) both derive it
  from `CARGO_PKG_VERSION`, and a compile-time check rejects any name longer than the 16-byte SH limit, so a
  pre-release version must be shortened first.
- Publishing needs a `CARGO_REGISTRY_TOKEN` repository secret (crates.io API token with `publish-new` and
  `publish-update` scopes). CI reads it from the environment; nothing is written to disk.
- Dependency order is load-bearing: a crate is publishable only after everything it depends on is on crates.io,
  so `release.yml` publishes `radicom-dict` → `radicom` → `radicom-json` → `radicom-network`. `xtask` is
  `publish = false`. `verify-tag` fails if a publishable crate's version differs from the tag or if the set of
  publishable crates no longer matches that list, so a new crate cannot be added and silently forgotten.
- Release notes are generated, never hand-written: after every crate is on crates.io, `release.yml` lists each
  non-merge commit subject between the previous tag and this one, plus a `compare` link. The first release lists
  the whole history instead. A `workflow_dispatch` run with a tag input regenerates those notes for an existing
  tag without publishing anything, which is how a release that predates the notes job is backfilled.
- The first public release was `v0.1.0`; the current release line is `v0.1.2`. Local `cargo package`
  verification for `radicom`, `radicom-json` and `radicom-network` cannot resolve their own unpublished
  dependencies; that is expected, and the tag run publishes the crates in dependency order.

## 7. Conventions

- Design changes flow **back** into [01-architecture.md](01-architecture.md) in the same commit as the code;
  these documents describe the current state, not history.
- Source comments reference sections as `docs/<file>.md §N`. Moving or renumbering a document means updating
  those references; `grep -r "docs/" crates xtask` finds them all.
- Source comments also carry stable work-item labels (`NET-06`, `SRV-04`, `PIX-02`, ...) from the original
  implementation plan. They are trace labels, not API contracts — the architecture sections are authoritative.
- Never weaken a test to make it pass; fix the cause.
