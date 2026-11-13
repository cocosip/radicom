# Changelog

## [Unreleased]

### Changed

- **Breaking:** `ContentItem::referenced_sop` and `ContentItem::waveform` take one `ReferencedSop` instead of
  a slice; PS3.3 C.18.3-C.18.5 allow exactly one Referenced SOP Sequence item.
- `DicomDirectory::add_file*` rejects paths that are not conformant File IDs (PS3.10 §8.2/§8.5: 1-8
  characters of `A-Z`, `0-9`, `_`, 1-8 components) instead of writing them verbatim.
- Strict validation rejects ACR-NEMA date/time forms (bare `±HHMM` DT offsets, `HH:MM:SS` TM), DEL in text
  VRs, undefined Specific Character Set terms, and VM counts that violate the multiplicity step.
- Presentation LUT Descriptor follows PS3.3 C.11.4.1: bit depth 10-16, first mapped value 0, one sequence
  item, entries high-aligned in 16-bit words.
- `BulkDataURI` is emitted only for the PS3.18 F.2.2 VRs; Native XML gains `xml:space="preserve"` and drops
  Group Length elements.

### Fixed

- Multi-frame 1-bit pixel data is bit-continuous across frames (PS3.5 §8.1.1 Note 2) in both frame reads and
  `add_frame`; 4:2:2 frame size uses the standard formula without column rounding.
- Explicit-VR UN with undefined length is read as an Implicit VR LE item sequence (PS3.5 §6.2.2, CP-246).
- In-memory pixel, LUT, SR and print buffers are host byte order (`from_ne_bytes`/`to_ne_bytes`), so big-endian
  hosts no longer misread them; 8/16-bit LUT descriptor counts, first mapped values and entry ranges follow
  PS3.3 C.11.2.1.1/C.11.4.1.
- Reader tolerates a nonzero delimiter/fragment-item length; writer rejects a defined length that collides
  with the undefined-length marker.
- Anonymization sets PatientIdentityRemoved and DeidentificationMethod and always syncs the file-meta SOP
  instance UID; DICOMDIR normalizes to Explicit VR Little Endian on load.
- DIMSE-N requests are accepted on any non-Verification presentation context (PS3.7 §10.1); PDV context IDs
  must be odd; Implementation Version Name is length-checked; the worklist return keys place
  ScheduledProcedureStepLocation inside its sequence; Film Size ID parses `8_5INX11IN` and `11INX17IN`.

## [0.2.0] - 2026-11-09

### Changed

- **Breaking:** `CommonExtended` requires `sop_class_uid`; `common_extended` on `ClientOptions` and
  `UserInformation` is now `Vec<CommonExtended>` instead of `Option<CommonExtended>`. See the README's
  migration section for updated initializers.
- The default receive spool limit is 4 GiB with a 1 MiB memory threshold. Incoming storage values retain
  their spool ownership; receive and deflate limits apply to memory and file paths.
- Message ID zero requests automatic allocation in high-level send APIs; the wire codec preserves it.
- Codec traits and the transcoder interface are unchanged. Companion codecs must depend on `radicom 0.2`
  to share this release's registry; `radicom-codecs 0.1.2` targets the `0.1` core.
- The DICOM **Implementation Version Name** now derives from the workspace version — `RADICOM 0.2.0` instead of
  the fixed `RADICOM_0_1` — in both file meta (0002,0013) and association user information, so a release bump
  updates the peer-visible identity automatically. A compile-time check rejects a name longer than the 16-byte
  SH limit.

### Added

- Concrete DIMSE request objects convert into `DicomRequest` for heterogeneous `DicomClient::send_requests`
  collections with response callbacks, bounded asynchronous windows and active-request cancellation.
- `with_reverse_store_classes` and `send_c_get_registered` declare C-GET storage classes once per client.

### Fixed

- DICOM UL item lengths, common and extended negotiation layouts, default roles, optional response UIDs,
  terminal retrieve counters, N-service Type IDs and Requested/Affected UID fields follow the standard.
- Request ID reservation prevents collisions across allocation, batches and abandoned request futures;
  cancellation state is reset when a later request reuses an ID.
- `Association::send_n_service` rejects C-service and response command fields before sending.
- A rejected C-STORE batch response releases its reserved Message ID after context or UID validation
  fails; wire-peer regressions cover malformed responses and interrupted operations across SCU services.
- Text VR multiplicity, character decoding, date/time validation, pixel and rendering edge cases, and safe
  serialization have additional conformance checks and regression coverage.
- README examples document current mixed requests, C-GET handling, cancellation and migration APIs.
- Tag releases enforce the 95% production-line coverage gate before publishing.
- Concurrent filesystem tests use distinct temporary directories to avoid deleting each other's fixtures.
- The large mixed-transfer test checks both terminal responses and the reverse store, with a deadlock
  watchdog separate from its two-second request inactivity timeout.
- Production coverage excludes explicitly gated external test modules as well as inline tests.

## [0.1.2] - 2026-11-05

### Changed

- `Transcoder::with_strict_dicom_vr(bool)` now selects only the encapsulated Pixel Data **write**
  mode, matching go-dicom's `WithStrictDICOMVR`: `true` is `Standard` (`OB`), `false` is `Compatible`
  (legacy fo-dicom 5.x `OW`). Reading stays at `with_pixel_data_read_mode` (tolerant by default), so
  asking for strict output no longer makes legacy `OW` inputs unreadable.

### Added

- `Dataset::clone_with_syntax_and_vr_mode` and `DicomFile::clone_with_syntax_and_vr_mode` expose the
  write VR mode (go-dicom `WithPixelDataWriteMode`), so callers can emit fo-dicom 5.x-compatible `OW`
  headers through the convenience API instead of building a `Transcoder` by hand.

## [0.1.1] - 2026-11-05

### Changed

- **Encapsulated Pixel Data now defaults to VR `OB`** for every Bits Allocated value, as PS3.5 §8.2
  requires. `PixelData::add_frame` previously chose `OW` for 16-bit samples, which matched the
  non-conformant output of fo-dicom 5.x. Codecs no longer rewrite the VR after appending frames.
- `PixelDataVrMode` and `PixelData::create_new_with_vr_mode` make the policy explicit:
  `Standard` (the default) always writes `OB`; `Compatible` keeps the legacy `OW` fragments for files
  that must stay byte-identical to fo-dicom 5.16.7 and earlier. `PixelData::create_new` is unchanged
  and means `Standard`.
- `Transcoder` reads tolerantly by default (`Compatible`: a non-conformant VR is warned about, never
  refused) and can validate strictly with `with_pixel_data_read_mode(PixelDataVrMode::Standard)` or
  `with_strict_dicom_vr(true)`. Its write mode defaults to `Standard`;
  `with_pixel_data_write_mode(PixelDataVrMode::Compatible)` writes the legacy `OW` fragments.
- Reading is unchanged: `OB` and `OW` fragment sequences both parse, and `PixelData::from_dataset`
  keeps the stored VR, so an `OW` file round-trips as `OW`.
