//! Codec interface contract, parameters and registry
//! (CODEC-01..03/05; docs/01-architecture.md §9, §3).
//!
//! The core ships **zero codecs** (divergence D10). Every implementation —
//! JPEG family, JPEG-LS, J2K, HTJ2K and DICOM RLE — lives in radicom-codecs
//! and registers itself through [`set_codec_registry`]. Without an injected
//! registry every encapsulated path fails fast with
//! [`CodecError::NotRegistered`].
//!
//! This module is the normative boundary surface: after M4 it is frozen and
//! changes follow semver-major.

use crate::error::{CodecError, Error};
use crate::model::transfer_syntax::TransferSyntax;
use crate::pixel::{PixelData, PixelDataRef};
use std::sync::{Arc, LazyLock, RwLock};

// -- parameters (CODEC-02, §2.2) -----------------------------------------

/// Chroma sample factor for JPEG encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SampleFactor {
    /// 4:4:4 — no chroma subsampling.
    #[default]
    Full,
    /// 4:2:2 — chroma halved horizontally.
    Half,
}

/// JPEG (Process 1/2&4) parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct JpegParams {
    /// 0..=100.
    pub quality: u32,
    pub smoothing_factor: u32,
    pub convert_colorspace_to_rgb: bool,
    pub sample_factor: SampleFactor,
    pub predictor: u32,
    pub point_transform: u32,
}

impl Default for JpegParams {
    fn default() -> Self {
        JpegParams {
            quality: 90,
            smoothing_factor: 0,
            convert_colorspace_to_rgb: false,
            sample_factor: SampleFactor::Full,
            predictor: 1,
            point_transform: 0,
        }
    }
}

/// JPEG 2000 parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct J2kParams {
    /// `true` = irreversible 9/7, `false` = reversible 5/3.
    pub irreversible: bool,
    /// Compression ratio.
    pub rate: u32,
    pub rate_levels: Vec<u32>,
    pub allow_mct: bool,
    pub update_photometric_interpretation: bool,
    pub encode_signed_as_unsigned: bool,
}

impl Default for J2kParams {
    fn default() -> Self {
        J2kParams {
            irreversible: true,
            rate: 20,
            rate_levels: Vec::new(),
            allow_mct: true,
            update_photometric_interpretation: false,
            encode_signed_as_unsigned: false,
        }
    }
}

/// Progression order for HTJ2K.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProgressionOrder {
    #[default]
    Lrcp,
    Rlcp,
    Rpcl,
    Pcrl,
    Cprl,
}

/// HTJ2K parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct HtJ2kParams {
    pub irreversible: bool,
    pub decompositions: u32,
    pub employ_color_transform: bool,
    pub progression_order: ProgressionOrder,
    pub insert_tlm_markers: bool,
}

impl Default for HtJ2kParams {
    fn default() -> Self {
        HtJ2kParams {
            irreversible: true,
            decompositions: 5,
            employ_color_transform: false,
            progression_order: ProgressionOrder::Lrcp,
            insert_tlm_markers: false,
        }
    }
}

/// JPEG-LS parameters. Opaque to the core: radicom-codecs finalizes the
/// fields; this type only carries them across the boundary.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct JpegLsParams {
    /// Maximum absolute error for near-lossless (0 = lossless).
    pub near_lossless: u32,
    /// 0 = none, 1 = planar, 2 = line interleaved, 3 = sample interleaved.
    pub interleave_mode: u32,
    /// 0 = none, 1 = HP1, 2 = HP2, 3 = HP3.
    pub color_transformation: u32,
    pub preset_coding_params: u32,
    pub reset_contexts: bool,
}

/// Codec parameters, one variant per codec family.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CodecParams {
    #[default]
    None,
    Jpeg(JpegParams),
    Jpeg2000(J2kParams),
    HtJpeg2000(HtJ2kParams),
    JpegLs(JpegLsParams),
}

impl CodecParams {
    /// The JPEG variant's parameters, if this is a JPEG parameter set.
    pub fn as_jpeg(&self) -> Option<&JpegParams> {
        match self {
            CodecParams::Jpeg(p) => Some(p),
            _ => None,
        }
    }

    pub fn as_j2k(&self) -> Option<&J2kParams> {
        match self {
            CodecParams::Jpeg2000(p) => Some(p),
            _ => None,
        }
    }

    pub fn as_htj2k(&self) -> Option<&HtJ2kParams> {
        match self {
            CodecParams::HtJpeg2000(p) => Some(p),
            _ => None,
        }
    }

    pub fn as_jpegls(&self) -> Option<&JpegLsParams> {
        match self {
            CodecParams::JpegLs(p) => Some(p),
            _ => None,
        }
    }
}

// -- codec trait (CODEC-01, §2.1) ----------------------------------------

/// A codec for one transfer syntax. Implementations live in
/// radicom-codecs only.
pub trait Codec: Send + Sync {
    /// Human-readable name, usually the transfer syntax name.
    fn name(&self) -> &str;

    /// The transfer syntax this codec encodes to / decodes from.
    fn transfer_syntax(&self) -> &'static TransferSyntax;

    fn default_params(&self) -> CodecParams {
        CodecParams::None
    }

    /// Encodes `src` frames into `dst`. Implementations must read every frame
    /// via `src.get_frame(i)`, write every frame via `dst.add_frame(...)`,
    /// touch no dataset element outside the `PixelData` setters, and map all
    /// failures to [`Error::Codec`] without panicking.
    fn encode(&self, src: &PixelDataRef<'_>, dst: &mut PixelData<'_>, params: &CodecParams) -> Result<(), Error>;

    /// Decodes `src` frames into `dst`, with the same obligations as
    /// [`Codec::encode`].
    fn decode(&self, src: &PixelDataRef<'_>, dst: &mut PixelData<'_>, params: &CodecParams) -> Result<(), Error>;
}

// -- registry (CODEC-03, §3) ---------------------------------------------

/// Resolves transfer syntaxes to codecs.
pub trait CodecRegistry: Send + Sync {
    fn has_codec(&self, syntax: &TransferSyntax) -> bool;

    /// True when both sides are non-encapsulated, or a codec exists for
    /// whichever side is encapsulated.
    fn can_transcode(&self, from: &TransferSyntax, to: &TransferSyntax) -> bool;

    /// The codec for `syntax`, or [`CodecError::NotRegistered`].
    fn get_codec(&self, syntax: &TransferSyntax) -> Result<Arc<dyn Codec>, Error>;
}

/// The empty default registry: the core ships zero codecs (D10).
#[derive(Debug, Default)]
pub struct EmptyCodecRegistry;

impl CodecRegistry for EmptyCodecRegistry {
    fn has_codec(&self, _syntax: &TransferSyntax) -> bool {
        false
    }

    fn can_transcode(&self, from: &TransferSyntax, to: &TransferSyntax) -> bool {
        !from.is_encapsulated() && !to.is_encapsulated()
    }

    fn get_codec(&self, syntax: &TransferSyntax) -> Result<Arc<dyn Codec>, Error> {
        Err(Error::Codec(CodecError::NotRegistered(syntax.uid().to_string())))
    }
}

/// Serializes tests that touch the global registry: it is process-wide by
/// design, so parallel test threads would otherwise observe each other's
/// injections.
#[cfg(test)]
pub(crate) static REGISTRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquires [`REGISTRY_TEST_LOCK`], recovering from a poisoned mutex (an
/// earlier failing assertion must not cascade into unrelated tests).
#[cfg(test)]
pub(crate) fn lock_registry() -> std::sync::MutexGuard<'static, ()> {
    REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
static REGISTRY: LazyLock<RwLock<Arc<dyn CodecRegistry>>> =
    LazyLock::new(|| RwLock::new(Arc::new(EmptyCodecRegistry)));

/// Replaces the global registry; idempotent.
pub fn set_codec_registry(registry: Arc<dyn CodecRegistry>) {
    *REGISTRY.write().expect("registry lock") = registry;
}

/// The current global registry (empty until [`set_codec_registry`] runs).
pub fn codec_registry() -> Arc<dyn CodecRegistry> {
    REGISTRY.read().expect("registry lock").clone()
}

/// Restores the empty registry. Test-only helper; harmless in production.
pub fn reset_codec_registry() {
    set_codec_registry(Arc::new(EmptyCodecRegistry));
}

// -- JPEG bit-depth scanning (CODEC-05) ----------------------------------

/// Marker bytes of interest when scanning a JPEG bitstream.
const SOF_MARKERS: [u8; 12] = [0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE];

/// Bit depth declared by a JPEG SOF marker (CODEC-05).
///
/// Codecs use this to detect the 12-bit codestreams that would otherwise be
/// mislabelled by the transfer syntax alone. Returns `None` for a
/// non-JPEG or truncated stream.
pub fn jpeg_bit_depth(data: &[u8]) -> Option<u8> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 3 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        // stand-alone markers carry no length field
        if (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if SOF_MARKERS.contains(&marker) {
            // length(2) + precision(1) + height(2) + width(2) + components(1)
            return data.get(i + 4).copied();
        }
        if len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::transfer_syntax::syntaxes;
    use crate::model::vr::Vr;
    use crate::model::dataset::Dataset;
    use crate::model::tag::Tag;
    use crate::pixel::{BITS_ALLOCATED, COLUMNS, PHOTOMETRIC_INTERPRETATION, ROWS};

    /// A trivial codec used to prove the boundary contract.
    struct MockCodec {
        syntax: &'static TransferSyntax,
    }

    impl Codec for MockCodec {
        fn name(&self) -> &str {
            self.syntax.name()
        }
        fn transfer_syntax(&self) -> &'static TransferSyntax {
            self.syntax
        }
        fn encode(&self, src: &PixelDataRef<'_>, dst: &mut PixelData<'_>, _p: &CodecParams) -> Result<(), Error> {
            for i in 0..src.number_of_frames() {
                let frame = src.get_frame(i)?;
                // mock "compression": XOR every byte so tests can tell the
                // payload apart from the source
                let mut bytes = frame.data().to_vec();
                for b in &mut bytes {
                    *b ^= 0xFF;
                }
                dst.add_frame(bytes)?;
            }
            Ok(())
        }
        fn decode(&self, src: &PixelDataRef<'_>, dst: &mut PixelData<'_>, _p: &CodecParams) -> Result<(), Error> {
            self.encode(src, dst, &CodecParams::None)
        }
    }

    struct MockRegistry {
        syntax: &'static TransferSyntax,
    }

    impl CodecRegistry for MockRegistry {
        fn has_codec(&self, syntax: &TransferSyntax) -> bool {
            syntax.uid() == self.syntax.uid()
        }
        fn can_transcode(&self, from: &TransferSyntax, to: &TransferSyntax) -> bool {
            (!from.is_encapsulated() || self.has_codec(from)) && (!to.is_encapsulated() || self.has_codec(to))
        }
        fn get_codec(&self, syntax: &TransferSyntax) -> Result<Arc<dyn Codec>, Error> {
            if self.has_codec(syntax) {
                Ok(Arc::new(MockCodec { syntax: self.syntax }))
            } else {
                Err(Error::Codec(CodecError::NotRegistered(syntax.uid().to_string())))
            }
        }
    }

    fn dataset(bits: u16, w: u32, h: u32) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(ROWS, h);
        ds.insert_value(COLUMNS, w);
        ds.insert_value(BITS_ALLOCATED, bits);
        ds.insert_str(PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
        ds
    }

    #[test]
    fn parameter_defaults_match_the_contract() {
        assert_eq!(JpegParams::default().quality, 90);
        assert_eq!(JpegParams::default().predictor, 1);
        assert_eq!(JpegParams::default().sample_factor, SampleFactor::Full);
        assert!(J2kParams::default().irreversible, "9/7 by default");
        assert_eq!(J2kParams::default().rate, 20);
        assert!(J2kParams::default().allow_mct);
        assert_eq!(HtJ2kParams::default().decompositions, 5);
        assert_eq!(HtJ2kParams::default().progression_order, ProgressionOrder::Lrcp);
        assert_eq!(JpegLsParams::default().near_lossless, 0, "lossless by default");
        assert_eq!(CodecParams::default(), CodecParams::None);
    }

    #[test]
    fn default_registry_is_empty() {
        let _guard = lock_registry();
        reset_codec_registry();
        let registry = codec_registry();
        assert!(!registry.has_codec(&syntaxes::JPEG_2000_LOSSLESS));
        match registry.get_codec(&syntaxes::JPEG_2000_LOSSLESS) {
            Err(Error::Codec(CodecError::NotRegistered(uid))) => {
                assert_eq!(uid, "1.2.840.10008.1.2.4.90", "error names the syntax");
            }
            Err(e) => panic!("expected NotRegistered, got {e}"),
            Ok(codec) => panic!("expected NotRegistered, got codec {}", codec.name()),
        }
    }

    #[test]
    fn can_transcode_truth_table() {
        let _guard = lock_registry();
        reset_codec_registry();
        let registry = codec_registry();
        let evle = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let j2k = syntaxes::JPEG_2000_LOSSLESS.clone();
        assert!(registry.can_transcode(&evle, &evle), "uncompressed both sides");
        assert!(!registry.can_transcode(&evle, &j2k), "no codec for the target");
        assert!(!registry.can_transcode(&j2k, &evle), "no codec for the source");

        set_codec_registry(Arc::new(MockRegistry { syntax: &syntaxes::JPEG_2000_LOSSLESS }));
        let registry = codec_registry();
        assert!(registry.has_codec(&j2k));
        assert!(registry.can_transcode(&evle, &j2k));
        assert!(registry.can_transcode(&j2k, &evle));
        assert!(registry.get_codec(&j2k).is_ok());
        reset_codec_registry();
    }

    #[test]
    fn injected_codec_round_trips_through_the_boundary() {
        let _guard = lock_registry();
        set_codec_registry(Arc::new(MockRegistry { syntax: &syntaxes::JPEG_2000_LOSSLESS }));
        let codec = codec_registry().get_codec(&syntaxes::JPEG_2000_LOSSLESS).unwrap();

        let mut source = dataset(8, 2, 2);
        source.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        {
            let mut pd = PixelData::from_dataset(&mut source).unwrap();
            pd.add_frame([1u8, 2, 3, 4]).unwrap();
        }

        let mut target = dataset(8, 2, 2);
        target.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        {
            let src = PixelDataRef::from_dataset(&source).unwrap();
            let mut dst = PixelData::from_dataset(&mut target).unwrap();
            codec.encode(&src, &mut dst, &codec.default_params()).unwrap();
        }
        // the mock flips every byte, so the payload is provably transformed
        let pd = PixelData::from_dataset(&mut target).unwrap();
        assert_eq!(pd.get_frame(0).unwrap().data().as_ref(), &[0xFE, 0xFD, 0xFC, 0xFB]);
        reset_codec_registry();
    }

    #[test]
    fn jpeg_bit_depth_scans_the_sof_marker() {
        // SOI, APP0 stub, then SOF0 with precision 12
        let mut data = vec![0xFF, 0xD8];
        data.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        data.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 12, 0x00, 0x10, 0x00, 0x10, 0x01]);
        assert_eq!(jpeg_bit_depth(&data), Some(12));

        // 8-bit baseline
        let mut data = vec![0xFF, 0xD8];
        data.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 8, 0x00, 0x10, 0x00, 0x10, 0x01]);
        assert_eq!(jpeg_bit_depth(&data), Some(8));
    }

    #[test]
    fn jpeg_bit_depth_rejects_non_jpeg_and_truncated_data() {
        assert_eq!(jpeg_bit_depth(b"not a jpeg"), None);
        assert_eq!(jpeg_bit_depth(&[0xFF, 0xD8]), None, "no marker yet");
        assert_eq!(jpeg_bit_depth(&[0xFF, 0xD8, 0xFF, 0xC0, 0x00]), None, "truncated SOF");
    }

    #[test]
    fn params_accessors_dispatch_by_variant() {
        let p = CodecParams::Jpeg(JpegParams { quality: 42, ..Default::default() });
        assert_eq!(p.as_jpeg().unwrap().quality, 42);
        assert!(p.as_j2k().is_none());
        assert!(CodecParams::None.as_jpeg().is_none());
    }

    #[test]
    fn tag_constants_are_usable_from_tests() {
        // keeps the imported tag constants meaningful for readers
        assert_eq!(Tag::BITS_ALLOCATED, BITS_ALLOCATED);
    }
}
