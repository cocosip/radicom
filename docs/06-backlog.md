# Backlog — deferred feature increments (2026-10-30)

Items deliberately left out of the 2026-10-29 defect-fix round. They are **feature additions or
environment/scope work**, not confirmed defects; the defect findings from that review are fixed.
Nothing here is a silent gap: each item names the current behaviour, the fo-dicom/standard reference,
and the acceptance test for the day it is implemented.

## 1. Streaming datasets (network)

- **Current:** a C-STORE/C-GET/C-MOVE message encodes the whole dataset into one `Vec<u8>` before the
  first P-DATA PDU (`message.rs::encode_dataset`); a transcoded reverse store holds the transcoded
  dataset *and* its encoded copy.
- **Reference:** fo-dicom `DicomService`/`PDataTFStream` streams the dataset through the writer; PS3.8
  P-DATA fragmentation does not require the whole dataset in memory.
- **Why deferred:** it changes the writer/splitter contract and needs back-pressure; it is not a
  correctness defect.
- **Acceptance:** C-STORE of a multi-GB instance runs in O(PDU) memory; a benchmark asserts peak
  allocation under a bound; wire bytes unchanged (goldens pass).

## 2. TLS system trust store and host-name verification

- **Current:** TLS needs an explicit CA bundle; a missing CA file yields an empty root store that only
  fails at handshake. The default identity is the peer IP, so DNS-name certificates fail unless
  `server_name` is set. `accept_any` is available without a guardrail. (Config-time guardrails and
  clearer key-parse errors are fixed in the defect round; the trust-store integration is not.)
- **Reference:** fo-dicom `Tls/DefaultTlsInitiator` uses the platform trust store and verifies the
  connected host name.
- **Why deferred:** needs a new optional dependency (`rustls-native-certs`-style) and platform testing.
- **Acceptance:** with no CA configured, a public-CA server connects; verification uses the DNS name
  when supplied; `accept_any` requires an explicit opt-in; tests use a generated CA on loopback.

## 3. JSON `BulkDataURI` for deferred buffers

- **Current:** an OW/OB stored as `FileWindow`/`StreamWindow` is materialised and base64-encoded for
  JSON (≈1.37× bytes plus the string). `BulkDataURI` is only emitted when the buffer already carries a
  URI (e.g. parsed from JSON).
- **Reference:** fo-dicom `JsonDicomConverter` + `BulkDataUriByteBuffer` (PS3.18 Annex F).
- **Why deferred:** needs a URI-provider callback in `JsonOptions` and a bundle writer; the current
  output is valid JSON, just memory-hungry for large on-demand values.
- **Acceptance:** with a URI provider configured, a `FileWindow` element serializes as
  `{"BulkDataURI": …}` without calling `data()`; a round-trip through a resolver still works.

## 4. Structured-report builders, typed readers and full validation

- **Current:** CONTAINER/TEXT/CODE/NUM/PNAME/DATE/TIME/DATETIME/UIDREF/COMPOSITE/IMAGE content items
  can be built; SCOORD, SCOORD3D, TCOORD and WAVEFORM cannot. Typed readers exist for text, measured
  value, referenced SOPs, concept name and children only. `StructuredReport::validate` checks the
  single-CONTAINER-root rule but not the SR IOD required attributes or per-item ValueType/value tag.
- **Reference:** PS3.3 C.18.1–C.18.8, C.17.3; fo-dicom `StructuredReport/DicomContentItem`.
- **Why deferred:** new API surface (builders + validation rules), not a defect in the shipped surface.
- **Acceptance:** all 15 value types buildable and readable; an SR missing CompletionFlag, a child
  RelationshipType or a value tag fails `validate()`; round-trip tests for SCOORD/SCOORD3D/TCOORD.

## 5. Print `SmoothingType` and `BorderDensity` — done (2026-10-30)

- **Shipped:** `FilmBox::smoothing_type`/`border_density`/`min_density`/`max_density` plus the
  image-box `smoothing_type` override. `render_to_image` paints the film area with BorderDensity, each
  image box area with the empty-image density, and filters each cell with the SmoothingType.
- **Reference:** PS3.3 Table C.13-3 (Border Density, Smoothing Type, Min/Max Density are all Film Box
  attributes) and Table C.13-5 (an image box Smoothing Type overrides the film box value).
- **Library choices** (the standard leaves them open): the recognised smoothing values are `SMOOTH`
  (3×3 box blur), `MEDIUM` (3×3 binomial) and `SHARP` (3×3 unsharp); Smoothing Type is defined only for
  `MagnificationType` CUBIC, which this renderer does not implement, so the filter is applied as a
  print-time enhancement. A numeric BorderDensity is hundredths of optical density (`150` = 1.5 OD).
- **Tests:** `a_density_string_parses_into_the_printed_grey`,
  `border_density_paints_the_film_area_between_images`, `smoothing_type_reaches_the_rendered_cell` and
  `smoothing_applies_the_documented_filter` in `crates/radicom/src/print/render.rs`.

## 6. Out of this repository (scope / platform / environment)

| Item | Reason |
|---|---|
| JPEG / JPEG-LS / JPEG 2000 / HTJ2K / RLE codecs | Lives in the separate `radicom-codecs` project; this workspace defines the `Codec` trait and registry |
| macOS MAC address reading | Needs CoreWLAN/IOKit FFI on a macOS host; Windows/Linux are implemented |
| DCMTK / fo-dicom reference-peer regression | Needs an external peer; loopback self-interop covers the same paths |
