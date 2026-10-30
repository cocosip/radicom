# Review Fixes and Evidence — 2026-10-30

This ledger closes the deep review in `target/2026-10-30-deep-review.md`. The concrete image codec
implementations remain outside this repository; codec interfaces, transcoding, rendering and metadata
propagation are in scope.

## Closed in code

- Core: ISO 2022 PN delimiter state, UTF-8-safe malformed DT/timezone errors, non-finite FL/FD JSON
  round trips, single-value LT/ST/UT backslashes and permitted multiline controls.
- Pixel/media/render: compressed frames are kept in their actual syntax until decoding; decoder metadata
  reaches the destination; registered decoders are used by public render paths; overlays use continuous
  bit positions and frame/origin bounds; MPR samples a singleton slice without indexing a second slice; print honours explicit sparse
  ImageBox positions; DICOMDIR writes valid consistency and zero links.
- SR: waveform channels are nested in each referenced SOP item; common document and value macro checks
  cover required identity/date/time/flags, verified observers, code VR/VM, references, coordinates,
  temporal values, and by-reference tree paths. Empty Type 2 NUM is accepted. Clinical TID and every
  SOP-specific relationship table are outside this common validator.
- Network: raw Deflated transfer syntax, context SOP/role checks, response count buckets and failed UID
  identifiers, zero Max PDU normalization, cancel-safe association reads, and body-only Maximum Length
  handling. Server syntax policy and the negotiated request window are exposed through the network API.
- Coverage: default invocation always runs `cargo llvm-cov`; explicit `--lcov` files must contain complete
  records, existing production sources and production lines. Duplicate records merge rather than inflate
  totals, and test-module scanning blanks comments/literals before matching braces.

## Finding closure map

IDs below are the 2026-10-30 deep-review IDs; they are distinct from the earlier CORE/NET/FEAT IDs.

| ID | Repaired behavior | Regression area |
|---|---|---|
| 1 | Compressed frame length is independent of native geometry | pixel/transcoder variable codec |
| 2 | Destination dimensions, bit depth, signedness, planar and colour metadata survive | transcoder and Big Endian save/reopen |
| 3 | JIS delimiter bytes and malformed pairs preserve reset boundaries | charset direct and DICOM file round trips |
| 4 | Malformed Unicode DT/timezone inputs return errors; UTC limits include minutes | model datetime/dataset |
| 5 | Buffered and streamed Deflated datasets contain completed raw DEFLATE streams | network/message |
| 6 | Negotiated context SOP and sender role are checked before dispatch | association/server loopback |
| 7 | Success/warning/failure counters are distinct; failed UID Identifiers are sent/read; terminal Remaining omitted | cget/cmove/server |
| 8 | Overlay bits continue across row boundaries | pixel/overlay and render |
| 9 | Overlay frame and frame-origin select the correct packed frame | pixel/overlay |
| 10 | A singleton MPR samples its own plane without indexing another slice | render/reconstruction |
| 11 | Explicit sparse print positions do not reuse an image in another cell | print/render |
| 12 | DICOMDIR consistency is zero and required absent links are UL zero | media/save and corpus |
| 13 | Waveform channels reside in the referenced SOP item and retain reference identity | SR raw nested reference input |
| 14 | Common SR document/type/conditional and imported value constraints are checked | SR validator and real corpus |
| 15 | Binary FL/FD NaN and infinities retain their values through JSON | JSON float round trips |
| 16 | Single text values keep literal backslashes and permitted multiline controls; raw/string validation agrees | element/validation/JSON/XML |
| 17 | A configured zero maximum PDU is normalized consistently for sending and advertising | network loopback |
| 18 | A cloneable signal cancels active FIND/GET/MOVE while the socket owner reads | client live loopback |
| 19 | Default coverage is fresh; invalid/empty/missing-source and alias-inflated reports cannot pass | xtask coverage tests |
| 20 | Negotiated Maximum Length excludes the six-byte PDU header | splitter and both transport readers at boundary |

Independent re-review also caught and repaired 8-to-16-bit codec storage VR, negative overlay origins,
out-of-range overlay shifts, XML numeric arrays, malformed JIS reset handling, raw string validation,
DT range fallback, SR expected VR/cardinality, and coverage source aliases/comment markers.

## API and migration notes

- `ServerOptions` accepts `supported_transfer_syntaxes`, `syntax_priority` and `async_operations`;
  `ClientOptions::async_operations` offers the requestor's window. Omitted async negotiation remains
  synchronous (1,1); configured unlimited windows are accepted with a bounded server capacity of 64.
- `Association::send_c_echo_batch` pipelines at most the accepted invoked window and 64 requests,
  checks unique nonzero IDs, and returns responses in request order. Other send methods remain sequential.
- Obtain `DicomClient::cancellation_handle()` before starting FIND/GET/MOVE, then signal the active message
  ID from another task. The association socket owner sends C-CANCEL and keeps its partial-PDU state.
- GET/MOVE responses expose `failed_sop_instance_uids`. MOVE providers can use
  `store_to_destination_with_failures`; the older delivery helper remains available.
- `OverlayData::frame_origin()` returns `u16`, matching DICOM US and preserving values above 32767.
  SR validation now rejects malformed inputs that previously passed. Use option/response constructors or
  struct defaults when updating explicit public struct literals for the added fields.
- Codec trait signatures are preserved. Codecs may update destination pixel metadata before appending
  frames; the facade uses the updated native bit depth, including the OW requirement for 16-bit pixels.

## Local evidence

The final source was independently re-reviewed. The remaining reverse C-STORE response-context/UID
and N-EVENT-REPORT role findings were reproduced, repaired and checked again by the independent reviewer.
No blocking finding remains in the reviewed changes. This does not certify every possible input.

| Check | Result |
|---|---|
| `cargo xtask coverage` (fresh all-feature workspace suite) | 1437 passed, 0 failed, 1 external-peer test ignored |
| Production coverage, workspace floor 95% | **95.32%** (20405/21406 lines) |
| Production coverage by crate | core 95.69%; JSON 96.08%; network 94.40% |
| `cargo test --workspace` | 1210 passed, 0 failed, 1 external-peer test ignored |
| `cargo clippy --workspace --all-features --all-targets -- -D warnings` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps` | Passed |
| `cargo +1.89.0 check --workspace --all-features --all-targets` | Passed; final network corrections rechecked on 1.89 |
| `cargo xtask gen-dict --check` and `git diff --check` | Passed |

The floor is workspace-wide, not a per-crate guarantee. Coverage was recollected after the final source
fixes; the cached report was not reused. The corrected tool rejects empty/invalid reports and source
aliases that inflated counts. Native-root TLS tests accept the precise empty-store diagnostic;
verification remains fail-closed and explicit CA roots, DNS verification and mTLS are tested on loopback.
No benchmark timing or remote CI run was performed in this round.

The original 17-item audit is separate from the later CORE/NET/FEAT list. The deep review reconfirmed
original items 3–9 and 11–17 against their existing behavior tests, with the additional boundary defects
in this ledger. Original items 1–2 and the remaining item 10 UTF-8 paths are repaired here. The final
workspace suite exercises both the older regressions and the new ones; this is evidence for those
triggers, not an exhaustive proof for every malformed input or device.

## Parity and limits

The implementation follows fo-dicom behavior where the Rust model and async transport are appropriate,
without translating inheritance or dependency injection literally. Concrete JPEG/JPEG-LS/JPEG 2000/HTJ2K/RLE
codecs, external DICOM peer interoperability, print/media hardware, and non-Windows platform evidence are
not established by local tests. The stateless compressed renderer currently decodes the full dataset once.
Client pipelining currently covers C-ECHO batches (at most 64 outstanding); other request kinds remain
sequential. The server accepts bounded negotiated windows while executing requests serially. This is
usable asynchronous-window support, not full mixed-service multiplexing equivalence with fo-dicom.
Receive-side dataset assembly still materializes a complete dataset (1 GiB per message); send-side
streaming does not imply streaming receive. Print CUBIC/LIN OD and macOS MAC support retain the existing
documented limits. The reference inspected was local fo-dicom commit
`7ea6d424d0b0e11ecf6a55e81a8ac58b05d5e3e2` (`5.2.6-101-g7ea6d424`), not a claim about the latest remote.
