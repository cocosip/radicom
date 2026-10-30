# Repository Guidelines

## Project Structure

This repository is a Rust workspace targeting Rust 1.89+:

- `crates/radicom` contains the synchronous core: DICOM data model, file I/O, pixel and rendering APIs, media, SR, printing, and anonymization.
- `crates/radicom-network` contains Tokio-based DIMSE networking, client/server support, and TLS.
- `crates/radicom-json` provides DICOM JSON and Native XML serialization.
- `crates/radicom-dict` owns generated tag, UID, and private-dictionary tables; do not edit generated files manually.
- `xtask` contains maintenance commands. Tests, benchmarks, and examples live beside each crate; shared fixtures are in `tests-data/`.
- Read `docs/01-architecture.md` before changing core behavior. `docs/` records development, interoperability, and review constraints.

## Build, Test, and Development Commands

Run these from the repository root:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-features
cargo test --workspace --all-features
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo doc --workspace --all-features --no-deps
cargo run --package xtask -- gen-dict --check
```

Use `cargo run --package xtask -- regen-goldens` only after reviewing an intentional golden change. `cargo run --package xtask -- coverage` runs the production-line coverage check and enforces the repository floor.

## Coding Style and Naming

Use `rustfmt` defaults, four-space indentation, idiomatic ownership, and `Result`/`Option` error handling. Public APIs and non-obvious invariants need rustdoc or a short comment. Keep the architecture synchronous in the core and asynchronous in the network crate; preserve the frozen codec boundary. Use PS3.6 keyword names for public DICOM constants (`Tag::PATIENT_NAME`, `uids::CT_IMAGE_STORAGE`) and `snake_case` for functions and modules.

## Testing Guidelines

Add focused unit or integration tests with the feature they exercise. Prefer real fixtures from `tests-data/` for parsing and round-trip behavior. Golden tests fail on drift; regenerate them only for reviewed changes. External-peer tests in `crates/radicom-network/tests/interop.rs` require a configured DICOM peer and are ignored by normal runs.

## Commits and Pull Requests

Use concise, imperative Conventional Commit subjects with the repository's emoji prefixes, such as `🐛 fix(network): handle ...` or `📝 docs: update ...`. Keep commits focused. Pull requests should explain behavior changes, link related issues when applicable, list validation commands and results, and update relevant docs or fixtures. Do not include generated dictionary output unless the source dictionary or generation logic changed.
