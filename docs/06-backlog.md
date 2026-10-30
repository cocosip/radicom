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

## 3. JSON `BulkDataURI` for deferred buffers — done (2026-10-30)

- **Shipped:** `JsonOptions::bulk_data_uri` takes a `BulkDataUriProvider`
  (`Fn(&Tag, &SharedBuffer) -> Option<String>`). It is consulted for binary elements whose buffer is
  not in memory, and a returned URI is written as `{"BulkDataURI": …}` without calling `data()`; an
  in-memory value still becomes `InlineBinary`, and a `None` falls back to it.
- **Reference:** PS3.18 Annex F; fo-dicom `JsonDicomConverter` + `BulkDataUriByteBuffer`.
- **Library choice:** the provider is asked only for deferred buffers, so existing output is unchanged
  until a caller opts in. Writing the referenced files is the caller's job (the URI is opaque here).
- **Tests:** `a_deferred_value_is_written_as_a_bulk_data_uri_without_materialising_it` in
  `crates/radicom-json/src/lib.rs` (one provider call, no `InlineBinary`, and a resolver-style
  round-trip through `BulkDataUri::with_data`).

## 4. Structured-report builders, typed readers and full validation — done (2026-10-30)

- **Shipped:** SCOORD, SCOORD3D, TCOORD and WAVEFORM builders (`ContentItem::scoord`,
  `scoord3d`, `tcoord`, `waveform`) with their macro attributes and construction-time checks; typed
  readers for all 15 value types (`ContentItem::value` returning `ContentValue`, plus the per-type
  accessors); `StructuredReport::validate` now checks the SR IOD Type 1 attributes and every content
  item.
- **Root model fixed with it:** the document dataset *is* the root content item (PS3.3 C.17.2), so
  `root_items`/`set_root_items` became `content_items`/`set_content_items` (the root container's
  children) and `set_root` hoists a root container's ValueType, document title and ContinuityOfContent
  into the dataset. A by-reference target (0040,DB73) carries no ValueType.
- **Reference:** PS3.3 C.17.2/C.17.3 (document content), C.18.1–C.18.9 (value macros); fo-dicom
  `StructuredReport`/`DicomContentItem`.
- **Library choices:** `SpatialCoordinates3D` requires the Type 1 `ReferencedFrameOfReferenceUID` and a
  closed POLYGON (C.18.9.1.2); point counts, finite coordinates and a parseable DT are rejected at
  construction rather than at write time.
- **Tests:** `every_value_type_builds_and_reads_back`, `graphic_values_reject_the_wrong_point_count`,
  `a_polygon_must_close_and_a_scoord3d_needs_a_frame_of_reference`,
  `temporal_coordinates_check_their_range_type`, `graphic_and_temporal_values_survive_a_write_and_read`,
  `graphic_and_temporal_values_survive_a_big_endian_round_trip`, `a_real_sr_document_validates` (the
  DCMTK reference SR validates) and the validation tests in `crates/radicom/src/sr/`.

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
