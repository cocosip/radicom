# Changelog

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
