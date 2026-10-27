# Test data corpus (INF-06)

A **minimal** DICOM sample set for parse and round-trip baselines
(`crates/radicom/tests/corpus.rs`). It mirrors the categories in
`docs/09-testing.md` §3 without pulling in fo-dicom's full test tree.

## Provenance

These files are copied, unmodified and with their original names, from the
**fo-dicom** project's test data:

- Source repository: <https://github.com/fo-dicom/fo-dicom>
- Source path: `Tests/FO-DICOM.Tests/Test Data/`
- License: **Microsoft Public License (MS-PL)** — see `fo-dicom-LICENSE.txt`
  (copied verbatim from `License.txt` at the fo-dicom repository root).
- Copyright (c) 2012-2021 fo-dicom contributors.

The MS-PL notice above is retained for this batch as required by the fo-dicom
license. Files are used here only as inert test fixtures.

## Contents

| Local path | Original name | Category (docs/09-testing.md §3) |
|---|---|---|
| `explicit-le/GH220.dcm` | `GH220.dcm` | syntax matrix — explicit VR LE |
| `explicit-le/GH487.dcm` | `GH487.dcm` | syntax matrix — explicit VR LE |
| `explicit-le/1403-OB.dcm` | `1403-OB.dcm` | syntax matrix — explicit VR LE (OB) |
| `explicit-le/GH178.dcm` | `GH178.dcm` | syntax matrix — explicit VR LE, deep SQ + private tags |
| `implicit-le/GH1958.dcm` | `GH1958.dcm` | syntax matrix — implicit VR LE |
| `deflated/Issue1097_deflated_sq.dcm` | `Issue1097_FailToOpenDeflatedFileWithSQ.dcm` | syntax matrix — deflated (raw RFC 1951) with SQ |
| `deflated/GH227.dcm` | `GH227.dcm` | syntax matrix — deflated (raw RFC 1951) |
| `charset/chrGB2312.dcm` | `charset/chrGB2312.dcm` | charsets — GB2312 (`ISO 2022 IR 58`) |
| `charset/chrJapMulti.dcm` | `charset/chrJapMulti.dcm` | charsets — ISO 2022 Japanese multi-byte |
| `charset/chrSQEncoding.dcm` | `charset/chrSQEncoding.dcm` | charsets — SQ with specific charset |
| `sr/test_SR.dcm` | `test_SR.dcm` | structures — SR document tree |
| `media/DICOMDIR` | `DICOMDIR` | media — DICOMDIR |
| `encapsulated/GH538-jpeg1.dcm` | `GH538-jpeg1.dcm` | encapsulated — JPEG baseline (metadata/decode-skip) |

## Baselines enforced

- **Parse**: every file parses without error (`every_corpus_file_parses`).
- **Byte stability**: parse → write → parse → write is a fixed point
  (`every_corpus_file_write_is_stable`) — a deterministic golden baseline.
- **Semantic round-trip**: parse → write → parse reproduces the dataset after
  stripping the redundant `(gggg,0000)` group-length elements
  (`every_corpus_file_round_trips_semantically`). Charset files are covered by
  the byte-stability baseline instead: a PN re-encoding can change the in-memory
  string representation without changing the on-wire bytes (docs §4 tolerance).

## Goldens

`tests-data/goldens/` holds committed snapshots produced by
`cargo xtask regen-goldens` from deterministic corpus files:

| Golden | Source | Produced from |
|---|---|---|
| `GH178.dump.txt` | `explicit-le/GH178.dcm` | dataset dump |
| `dicomdir.dump.txt` | `media/DICOMDIR` | dataset dump (includes record offsets) |
| `GH178.json` | `explicit-le/GH178.dcm` | DICOM JSON |
| `associate_rq.hex` | — (synthetic) | A-ASSOCIATE-RQ PDU bytes |
| `pdata_cecho.hex` | — (synthetic) | P-DATA-TF carrying a C-ECHO-RQ command PDV |

`crates/radicom/tests/goldens.rs`, `crates/radicom-json/tests/goldens.rs` and
`crates/radicom-network/tests/goldens.rs` fail when a golden drifts; regenerate
only after reviewing an intended change. PDU hex fixtures ignore whitespace, so
only the encoded byte sequence is pinned.

## Adding files

Keep the set minimal and small. Add a row above, extend the `CORPUS` manifest in
`crates/radicom/tests/corpus.rs`, and retain the MS-PL notice for the batch.
