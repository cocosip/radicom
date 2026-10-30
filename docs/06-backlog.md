# Backlog — deferred feature increments (2026-10-30)

Items deliberately left out of the 2026-10-29 defect-fix round. They are **feature additions or
environment/scope work**, not confirmed defects; the defect findings from that review are fixed.
Nothing here is a silent gap: each item names the current behaviour, the fo-dicom/standard reference,
and the acceptance test for the day it is implemented.

## 1. Streaming datasets (network) — done (2026-10-30)

- **Shipped:** `message::write_message_streamed` sends the command, then encodes the dataset on a
  blocking thread into a bounded channel (capacity 2) that the association task writes out, so peak
  memory is O(PDU) no matter the instance size. `PduFramer` frames the writer's output into P-DATA PDUs
  one PDV at a time and holds back the tail so the final PDV carries the "last" bit. The C-STORE send
  path uses it whenever no transcode is needed.
- **Reference:** PS3.8 §9.3 P-DATA fragmentation; fo-dicom `DicomService`/`PDataTFStream`.
- **Library choices:** one PDV per PDU (the buffered splitter packs several) — more PDUs, same bytes;
  a transcoded C-STORE still materialises its transcoded output, which is inherent to transcoding.
- **Tests:** `the_framer_holds_at_most_one_pdu_and_marks_the_last_pdv` (asserts `max_buffered()` never
  exceeds one PDV), `a_streamed_message_matches_the_buffered_bytes_on_the_wire` and
  `a_large_dataset_streams_through_many_pdus_intact` (2 MiB over max_pdu 1024, reassembled
  byte-for-byte) in `crates/radicom-network/src/message.rs`; the loopback C-STORE interop test now
  exercises the streamed path.

## 2. TLS system trust store and host-name verification — done (2026-10-30)

- **Shipped:** with no CA bundle the client now trusts the platform root store (Windows certificate
  store, macOS keychain, OpenSSL default paths) via `rustls-native-certs`, so a public-CA server
  connects with no configuration. A configured bundle pins the anchors to exactly those certificates.
  Verification uses `server_name` when supplied and the peer address otherwise; `accept_any` logs a
  warning and still needs an explicit opt-in. An empty platform store is reported instead of silently
  verifying nothing.
- **Reference:** PS3.8 §9.3 TLS; fo-dicom `Tls/DefaultTlsInitiator` (`X509Store` + host-name check).
- **Library choice:** the trust source is platform-first, bundle-second — a bundle narrows trust rather
  than adding to it, matching a pinned-CA deployment.
- **Tests:** `a_ca_less_client_uses_the_platform_trust_store`,
  `the_platform_trust_store_is_loaded_or_reported` and
  `a_dns_certificate_is_verified_against_the_supplied_server_name` (a generated rcgen certificate on
  loopback: rejected against the IP, accepted once the DNS name is supplied) in
  `crates/radicom-network/src/tls.rs`.

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
