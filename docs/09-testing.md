# Test Strategy and Quality Assurance

> Status: design phase (applies from milestone M0 onward; see [03-development-plan.md](03-development-plan.md) §3)
> Feature IDs covered: INF-06; test requirements referenced by module specs 04–08
> Behavior baseline: fo-dicom `Tests/FO-DICOM.Tests` (data set organization, tolerance cases), `Tests/FO-DICOM.Benchmark`, layered codec-test DI collections

---

## 1. Principles

1. **The tolerance corpus is a first-class deliverable**: every lenient behavior (RD-03 T1–T10, DICOMDIR nearest-offset, JSON tolerance rules) has a named test with a comment referencing the originating fo-dicom issue or standard clause.
2. **Round-trips are the backbone**: read → write → re-read must be byte- or semantics-equal across the syntax matrix.
3. **Interop, not just self-consistency**: network and codec layers are tested against external reference peers (DCMTK, fo-dicom) in nightly CI.
4. **No panic guarantees**: parsing and network decode paths are fuzzed; all failures are typed errors.

## 2. Test Organization

```
crates/radicom/tests/           # integration tests per area (model, io, pixel, media...)
crates/radicom/src/**/tests.rs  # unit tests beside the code
crates/radicom-network/tests/   # loopback + golden PDU
crates/radicom-json/tests/      # JSON goldens
tests-data/                     # shared binary fixtures (see §3), referenced by relative path
fuzz/                           # cargo-fuzz targets (reader, pdu)
```

Test naming: `<area>_<behavior>__<case>` for easy triage; every tolerance test ends with the rule ID (e.g. `reader_un_vr_sq_recovery__T3`).

## 3. Test Data Inventory (INF-06)

Mirrors the categories fo-dicom's test data covers; import the minimal set (fo-dicom's test data ships under the repository's **MS-PL** license — retain the notice per file batch and record provenance in `tests-data/README.md`):

| Category | Examples needed |
|---|---|
| Syntax matrix | explicit LE, implicit LE, explicit BE, deflated |
| Encapsulated | RLE-encoded (facade/offset-table tests here; codec round-trips in the radicom-codecs layer), one JPEG/J2K sample (decode-skip tests only until radicom-codecs exists) |
| Malformed corpus | one file per RD-03 row T1–T10 (synthesized where no fixture exists — generators checked in as code) |
| Multi-frame | multiframe WSI-style (many frames), enhanced MR with functional groups |
| Charsets | CJK patient names in ISO 2022 multi-charset, GBK, GB18030, UTF-8 |
| Structures | nested/deep SQ, fragments with/without offset table, private tags incl. re-registered creators |
| Media | a small DICOMDIR file set (4 patients × 2 studies × 2 series) |
| SR | a complete SR document tree |
| Meta variants | no-preamble, no-meta, ACR-NEMA sample |
| Network | golden PDU byte captures (checked in as hex fixtures) |

## 4. Round-Trip and Golden Testing

- **Byte-level round-trip**: parse → write with identical options → byte-equal file. Applies to the syntax matrix and structure categories.
- **Semantic round-trip**: where byte equality is impossible (group-length removal, padding choices), compare via dataset deep equality (DS-07).
- **Goldens**: generated once, reviewed, then enforced: JSON outputs, DICOMDIR offsets, dump strings, PDU byte streams. (RLE frame goldens are enforced in the radicom-codecs project.)
- Golden regeneration is an explicit `cargo xtask regen-goldens` (fail CI if diff is unexpected).

## 5. Network Testing

- **Loopback**: in-repo SCU↔SCP integration tests for echo/store/find/get/move; server lifecycle (bind/shutdown/re-bind); client retry/linger/breaker scenarios with a scripted mock peer (a test-only `DicomService` implementation that replays PDU fixtures).
- **Reference interop (nightly, optional runners)**: against `storescp`/`storescu`/`echoscu` from DCMTK and a small fo-dicom host; matrix: association negotiation, large-object streaming, async ops, TLS.
- **Fuzz**: mutated PDU streams must produce `NetworkError` without panics or unbounded reads.

## 6. Codec Testing Layers

Two registry configurations, same test suite (mirrors fo-dicom's DI-collection pattern):

1. **default (empty) + mock codec**: the core ships no codecs — validates the PixelData facade, the transcoder decision table, missing-codec error paths, and the boundary contract itself via an in-repo **test-only mock codec crate** (also exercises error paths: failing encode/decode, wrong frame counts);
2. **codecs**: `set_codec_registry(NativeCodecRegistry)` from a dev-dependency on radicom-codecs — validates real end-to-end transcoding incl. RLE (separate job; skipped gracefully when the codecs crate is absent so the core repo CI never hard-depends on it).

## 7. Fuzzing and Robustness

- `fuzz/reader`: byte streams → `DicomReader` must terminate with `Success|Stopped|Error` (hang/panic = bug; depth guard covers recursion).
- `fuzz/pdu`: random PDUs into the engine.
- Memory guarantees: `ReadLargeOnDemand` windows never load whole values; values above the array cap become composites; C-STORE receive bounded by spooling (assert with allocation counters in one dedicated test).

## 8. Benchmarks (criterion, M8 but seeded early)

> Implementation note: `criterion` is not vendored in this workspace's offline
> cache, so the suite is a set of dependency-free `harness = false` binaries that
> time each operation with `std::time::Instant`. They are split by crate:
> `crates/radicom/benches/core.rs` (parse_dataset, write_dataset, get_frame, and
> frame_render under `--features render`) run with `cargo bench -p radicom`;
> `crates/radicom-json/benches/json.rs` (dataset_to_json, json_to_dataset,
> json_roundtrip) with `cargo bench -p radicom-json`; and
> `crates/radicom-network/benches/throughput.rs` (network_throughput, loopback
> C-STORE MiB/s) with `cargo bench -p radicom-network`.

| Benchmark | Measures |
|---|---|
| `parse_dataset` | files from the syntax matrix; explicit/implicit; large SQ |
| `write_dataset` | same corpus, write to sink |
| `get_frame` | uncompressed slicing vs encapsulated offset-table aggregation |
| `mock_codec_decode` | transcode path through the test-only mock codec (real codec benchmarks live in the radicom-codecs project) |
| `json_roundtrip` | dataset → JSON → dataset |
| `network_throughput` | loopback C-STORE of a multi-frame file (bytes/s) |

Each benchmark pins a baseline file; regressions >10% flagged in CI (nightly).

## 9. CI Stages

| Stage | Trigger | Content |
|---|---|---|
| fmt/clippy | every push | rustfmt + clippy `-D warnings` |
| test | every push | unit + integration (default registry only) |
| codecs-test | every push (if radicom-codecs available) | layered suite §6.2 |
| fuzz | nightly, time-boxed | both fuzz targets |
| interop | nightly, self-hosted runner | DCMTK/fo-dicom matrix §5 |
| bench | nightly or on tag | criterion compare vs baseline |

## 10. Definition of Done (per feature group)

A feature group (e.g. "Reader") is Done when:
1. all its feature-list items implemented and unit-tested;
2. every rule/table row of its module spec has a corresponding passing test (tolerance rows included);
3. round-trip/golden tests updated and green;
4. progress table in [03-development-plan.md](03-development-plan.md) §3 updated with date + notes;
5. module spec updated where implementation deviated (spec is living).
