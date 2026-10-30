//! Transcoder (TR-01..03; docs/01-architecture.md §9).
//!
//! Decides how to move pixel data between transfer syntaxes, resolves codecs
//! lazily through the global registry, and keeps the lossy-compression
//! bookkeeping correct. Codecs themselves never live here.

use crate::error::{CodecError, Error};
use crate::model::buffer::SharedBuffer;
use crate::model::dataset::Dataset;
use crate::model::item::{DicomFragmentSequence, DicomItem};
use crate::model::tag::Tag;
use crate::model::transfer_syntax::TransferSyntax;
use crate::model::vr::Vr;
use crate::pixel::codec::{codec_registry, CodecParams};
use crate::pixel::overlay::extract_overlays;
use crate::pixel::{PixelData, PixelDataRef, PIXEL_DATA};
use std::sync::Arc;

/// `(0028,2110)` Lossy Image Compression.
pub const LOSSY_IMAGE_COMPRESSION: Tag = Tag::LOSSY_IMAGE_COMPRESSION;
/// `(0028,2112)` Lossy Image Compression Ratio.
pub const LOSSY_IMAGE_COMPRESSION_RATIO: Tag = Tag::LOSSY_IMAGE_COMPRESSION_RATIO;
/// `(0028,2114)` Lossy Image Compression Method.
pub const LOSSY_IMAGE_COMPRESSION_METHOD: Tag = Tag::LOSSY_IMAGE_COMPRESSION_METHOD;

/// Moves pixel data from one transfer syntax to another (TR-01).
#[derive(Debug, Clone)]
pub struct Transcoder {
    from: Arc<TransferSyntax>,
    to: Arc<TransferSyntax>,
    in_params: Option<CodecParams>,
    out_params: Option<CodecParams>,
}

impl Transcoder {
    /// Builds a transcoder; codecs are resolved lazily at use time (§9.1).
    /// Omitted parameters use the codec's defaults; `Some(CodecParams::None)`
    /// explicitly passes no parameters.
    pub fn new(
        from: Arc<TransferSyntax>,
        to: Arc<TransferSyntax>,
        in_params: Option<CodecParams>,
        out_params: Option<CodecParams>,
    ) -> Self {
        Transcoder {
            from,
            to,
            in_params,
            out_params,
        }
    }

    pub fn from_syntax(&self) -> &TransferSyntax {
        &self.from
    }

    pub fn to_syntax(&self) -> &TransferSyntax {
        &self.to
    }

    /// Transcodes a dataset, returning a new one (§9.1 decision table).
    ///
    /// The input is never mutated: every path works on a clone.
    pub fn transcode_dataset(&self, ds: &Dataset) -> Result<Dataset, Error> {
        let mut source = ds.clone();
        if !ds.contains(&PIXEL_DATA) {
            source.set_transfer_syntax(self.to.clone());
            return Ok(source);
        }
        if !self.from.is_encapsulated() {
            extract_overlays(&mut source)?;
        }
        let original_bytes = PixelDataRef::from_dataset(ds)?.uncompressed_frame_size()?;
        let mut out = match (self.from.is_encapsulated(), self.to.is_encapsulated()) {
            (false, false) => {
                let frames = self.read_frames(&source)?;
                let mut out = source;
                out.set_transfer_syntax(self.to.clone());
                out.remove(PIXEL_DATA);
                out.remove(Tag::NUMBER_OF_FRAMES);
                let mut pixel = PixelData::create_new(&mut out)?;
                for frame in frames {
                    pixel.add_frame_buffer(frame)?;
                }
                out
            }
            (true, false) => self.decode_to_uncompressed(&source)?,
            (false, true) => self.encode_dataset(&source)?,
            (true, true) => self.encode_dataset(&self.decode_to_uncompressed(&source)?)?,
        };
        out.set_transfer_syntax(self.to.clone());
        let encoded_bytes = PixelDataRef::from_dataset(&out)?.get_frame(0)?.size();
        self.finish(&mut out)?;
        // (0028,2110) marks the output lossy when the target syntax compresses
        // with loss (PS3.3 C.7.6.4; fo-dicom Transcoder write-lossy bookkeeping).
        if self.to.is_lossy() {
            let method = self.to.lossy_compression_method().unwrap_or("UNKNOWN");
            Self::mark_lossy(&mut out, method, original_bytes, encoded_bytes);
        }
        Ok(out)
    }

    /// Convenience: transcode and clone a file (TR-03).
    pub fn transcode_file(
        &self,
        file: &crate::io::file::DicomFile,
    ) -> Result<crate::io::file::DicomFile, Error> {
        let dataset = self.transcode_dataset(&file.dataset)?;
        let mut out = file.clone();
        out.dataset = dataset;
        // the meta group's transfer syntax is refreshed on save
        out.file_meta
            .insert_str(Tag::TRANSFER_SYNTAX_UID, Vr::Ui, self.to.uid());
        Ok(out)
    }

    /// Decodes one frame without mutating the source (TR-02, §9.1).
    pub fn decode_frame(
        &self,
        ds: &Dataset,
        frame: u32,
    ) -> Result<crate::model::buffer::SharedBuffer, Error> {
        if !self.from.is_encapsulated() {
            return PixelDataRef::from_dataset(ds)?.get_frame(frame);
        }
        let codec = codec_registry().get_codec(&self.from)?;
        let bytes = self.read_frame(ds, frame)?;

        // Keep the isolated frame encapsulated: its length is codec-defined.
        let mut source = ds.clone();
        source.remove(PIXEL_DATA);
        source.remove(Tag::NUMBER_OF_FRAMES);
        PixelData::create_new(&mut source)?.add_frame_buffer(bytes)?;
        let mut decoded = source.clone();
        decoded.set_transfer_syntax(
            crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
        );
        decoded.remove(PIXEL_DATA);
        decoded.remove(Tag::NUMBER_OF_FRAMES);
        {
            let src = PixelDataRef::from_dataset(&source)?;
            let mut dst = PixelData::create_new(&mut decoded)?;
            let params = self
                .in_params
                .clone()
                .unwrap_or_else(|| codec.default_params());
            codec.decode(&src, &mut dst, &params)?;
        }
        PixelDataRef::from_dataset(&decoded)?.get_frame(0)
    }

    // -- flows ---------------------------------------------------------------

    /// Every frame of `ds` as a zero-copy shared buffer, in order.
    ///
    /// The frame is a window over the dataset's pixel buffer, so no per-frame
    /// `to_vec()` materialisation is needed; `add_frame` copies once into the
    /// output when it is written.
    fn read_frames(&self, ds: &Dataset) -> Result<Vec<SharedBuffer>, Error> {
        let pd = PixelDataRef::from_dataset(ds)?;
        let count = pd.number_of_frames();
        let mut frames = Vec::with_capacity(count as usize);
        for i in 0..count {
            frames.push(pd.get_frame(i)?);
        }
        Ok(frames)
    }

    /// One frame of `ds` as a zero-copy shared buffer.
    fn read_frame(&self, ds: &Dataset, frame: u32) -> Result<SharedBuffer, Error> {
        PixelDataRef::from_dataset(ds)?.get_frame(frame)
    }

    /// Row 4: preserve the complete codec destination, including pixel metadata.
    fn encode_dataset(&self, ds: &Dataset) -> Result<Dataset, Error> {
        let codec = codec_registry().get_codec(&self.to)?;
        let mut encoded = ds.clone();
        encoded.set_transfer_syntax(self.to.clone());
        encoded.remove(PIXEL_DATA);
        encoded.remove(Tag::NUMBER_OF_FRAMES);
        {
            let src = PixelDataRef::from_dataset(ds)?;
            let mut dst = PixelData::create_new(&mut encoded)?;
            let params = self
                .out_params
                .clone()
                .unwrap_or_else(|| codec.default_params());
            codec.encode(&src, &mut dst, &params)?;
        }
        Ok(encoded)
    }

    /// Decode the original encapsulated source into a native destination.
    fn decode_to_uncompressed(&self, ds: &Dataset) -> Result<Dataset, Error> {
        let codec = codec_registry().get_codec(&self.from)?;
        let mut decoded = ds.clone();
        decoded.set_transfer_syntax(
            crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
        );
        decoded.remove(PIXEL_DATA);
        decoded.remove(Tag::NUMBER_OF_FRAMES);
        {
            let src = PixelDataRef::from_dataset(ds)?;
            let mut dst = PixelData::create_new(&mut decoded)?;
            let params = self
                .in_params
                .clone()
                .unwrap_or_else(|| codec.default_params());
            codec.decode(&src, &mut dst, &params)?;
        }
        extract_overlays(&mut decoded)?;
        Ok(decoded)
    }

    /// §9.1 bookkeeping: encapsulated VR semantics and overlay extraction.
    fn finish(&self, out: &mut Dataset) -> Result<(), Error> {
        // encapsulated output always stores pixel data as OB
        if self.to.is_encapsulated() {
            let existing = out.item(&PIXEL_DATA).cloned();
            if let Some(DicomItem::Fragments(f)) = existing {
                if f.vr() != Vr::Ob {
                    let mut ob = DicomFragmentSequence::new(f.tag().clone(), Vr::Ob);
                    for fragment in f.fragments() {
                        ob.push_fragment(fragment.clone());
                    }
                    ob.set_offset_table(f.offset_table().to_vec());
                    out.insert_or_update(DicomItem::Fragments(ob));
                }
            }
        }
        Ok(())
    }

    /// Records lossy compression on `out` (§9.1), given the original and
    /// encoded first-frame sizes.
    pub fn mark_lossy(out: &mut Dataset, method: &str, original_bytes: u64, encoded_bytes: u64) {
        out.insert_str(LOSSY_IMAGE_COMPRESSION, Vr::Cs, "01");
        // append rather than replace, one entry per compression generation
        let mut methods = out
            .try_get_str(LOSSY_IMAGE_COMPRESSION_METHOD)
            .map(|s| split_multi(&s))
            .unwrap_or_default();
        methods.push(method.to_string());
        out.insert_str(LOSSY_IMAGE_COMPRESSION_METHOD, Vr::Lo, methods.join("\\"));

        let ratio = if encoded_bytes == 0 {
            0.0
        } else {
            original_bytes as f64 / encoded_bytes as f64
        };
        let mut ratios = out
            .try_get_str(LOSSY_IMAGE_COMPRESSION_RATIO)
            .map(|s| split_multi(&s))
            .unwrap_or_default();
        ratios.push(format!("{ratio:.3}"));
        out.insert_str(LOSSY_IMAGE_COMPRESSION_RATIO, Vr::Ds, ratios.join("\\"));
    }
}

/// Splits a multi-valued string, dropping empty components.
fn split_multi(value: &str) -> Vec<String> {
    value
        .split('\\')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Ensures the output syntax has a codec, failing fast otherwise (§9.1).
pub fn require_codec(syntax: &TransferSyntax) -> Result<(), Error> {
    if syntax.is_encapsulated() && !codec_registry().has_codec(syntax) {
        return Err(Error::Codec(CodecError::NotRegistered(
            syntax.uid().to_string(),
        )));
    }
    Ok(())
}

/// TR-03: a transcoded copy of a dataset (the source is never mutated).
impl Dataset {
    pub fn clone_with_syntax(
        &self,
        syntax: Arc<TransferSyntax>,
        out_params: Option<CodecParams>,
    ) -> Result<Dataset, Error> {
        Transcoder::new(self.transfer_syntax(), syntax, None, out_params).transcode_dataset(self)
    }
}

/// TR-03: a transcoded copy of a file (the source is never mutated).
impl crate::io::file::DicomFile {
    pub fn clone_with_syntax(
        &self,
        syntax: Arc<TransferSyntax>,
        out_params: Option<CodecParams>,
    ) -> Result<crate::io::file::DicomFile, Error> {
        Transcoder::new(self.dataset.transfer_syntax(), syntax, None, out_params)
            .transcode_file(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::buffer::MemoryBuffer;
    use crate::model::transfer_syntax::syntaxes;
    use crate::pixel::codec::{set_codec_registry, Codec, CodecRegistry};
    use crate::pixel::{BITS_ALLOCATED, COLUMNS, PHOTOMETRIC_INTERPRETATION, ROWS};

    // Contract fake: a two-byte run or six-byte literal becomes one RGB pixel.
    struct VariableCodec {
        syntax: &'static TransferSyntax,
    }

    impl Codec for VariableCodec {
        fn name(&self) -> &str {
            "variable frame contract"
        }
        fn transfer_syntax(&self) -> &'static TransferSyntax {
            self.syntax
        }
        fn default_params(&self) -> CodecParams {
            CodecParams::Jpeg2000(crate::pixel::codec::J2kParams::default())
        }
        fn encode(
            &self,
            src: &PixelDataRef<'_>,
            dst: &mut PixelData<'_>,
            params: &CodecParams,
        ) -> Result<(), Error> {
            assert!(!src.transfer_syntax().is_encapsulated());
            assert_eq!(dst.transfer_syntax().uid(), self.syntax.uid());
            assert_eq!(params, &self.default_params());
            let pi = if src.samples_per_pixel() == 3 {
                "YBR_FULL"
            } else {
                "MONOCHROME1"
            };
            dst.dataset_mut()
                .insert_str(PHOTOMETRIC_INTERPRETATION, Vr::Cs, pi);
            for i in 0..src.number_of_frames() {
                let frame = src.get_frame(i)?;
                let bytes = frame.data()?;
                dst.add_frame([bytes[0], 0])?;
            }
            Ok(())
        }
        fn decode(
            &self,
            src: &PixelDataRef<'_>,
            dst: &mut PixelData<'_>,
            params: &CodecParams,
        ) -> Result<(), Error> {
            assert!(src.transfer_syntax().is_encapsulated());
            assert_eq!(src.transfer_syntax().uid(), self.syntax.uid());
            assert_eq!(params, &self.default_params());
            let ds = dst.dataset_mut();
            ds.insert_value(ROWS, 1u16);
            ds.insert_value(COLUMNS, 1u16);
            ds.insert_value(Tag::SAMPLES_PER_PIXEL, 3u16);
            ds.insert_value(Tag::BITS_ALLOCATED, 8u16);
            ds.insert_value(Tag::BITS_STORED, 8u16);
            ds.insert_value(Tag::HIGH_BIT, 7u16);
            ds.insert_value(Tag::PLANAR_CONFIGURATION, 0u16);
            ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
            ds.insert_str(PHOTOMETRIC_INTERPRETATION, Vr::Cs, "RGB");
            for i in 0..src.number_of_frames() {
                let frame = src.get_frame(i)?;
                let bytes = frame.data()?;
                assert!(bytes.as_ref() == [10, 0] || bytes.as_ref() == [40, 1, 2, 3, 4, 5]);
                dst.add_frame([bytes[0], 20, 30])?;
            }
            Ok(())
        }
    }
    struct VariableRegistry;
    impl CodecRegistry for VariableRegistry {
        fn has_codec(&self, syntax: &TransferSyntax) -> bool {
            [
                syntaxes::JPEG_2000_LOSSLESS.uid(),
                syntaxes::RLE_LOSSLESS.uid(),
            ]
            .contains(&syntax.uid())
        }
        fn can_transcode(&self, from: &TransferSyntax, to: &TransferSyntax) -> bool {
            (!from.is_encapsulated() || self.has_codec(from))
                && (!to.is_encapsulated() || self.has_codec(to))
        }
        fn get_codec(&self, syntax: &TransferSyntax) -> Result<Arc<dyn Codec>, Error> {
            let syntax = [&*syntaxes::JPEG_2000_LOSSLESS, &*syntaxes::RLE_LOSSLESS]
                .into_iter()
                .find(|s| s.uid() == syntax.uid())
                .ok_or_else(|| Error::Codec(CodecError::NotRegistered(syntax.uid().into())))?;
            Ok(Arc::new(VariableCodec { syntax }))
        }
    }
    fn variable_compressed() -> Dataset {
        let mut ds = dataset(16, 1, 2);
        ds.insert_value(Tag::BITS_STORED, 12u16);
        ds.insert_value(Tag::HIGH_BIT, 11u16);
        ds.insert_value(Tag::PLANAR_CONFIGURATION, 1u16);
        ds.insert_value(Tag::PIXEL_REPRESENTATION, 1u16);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        let mut pixel = PixelData::create_new(&mut ds).unwrap();
        pixel.add_frame([10, 0]).unwrap();
        pixel.add_frame([40, 1, 2, 3, 4, 5]).unwrap();
        ds
    }
    #[test]
    fn variable_compressed_frames_keep_the_source_representation_and_decoded_metadata() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(VariableRegistry));
        let source = variable_compressed();
        let tr = Transcoder::new(
            source.transfer_syntax(),
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            None,
            None,
        );
        let out = tr.transcode_dataset(&source).unwrap();
        assert_eq!(out.get_str(PHOTOMETRIC_INTERPRETATION).unwrap(), "RGB");
        assert_eq!(out.get::<u16>(Tag::SAMPLES_PER_PIXEL).unwrap(), 3);
        assert_eq!(out.get::<u16>(Tag::BITS_ALLOCATED).unwrap(), 8);
        assert_eq!(out.get::<u16>(Tag::BITS_STORED).unwrap(), 8);
        assert_eq!(out.get::<u16>(Tag::HIGH_BIT).unwrap(), 7);
        assert_eq!(out.get::<u16>(Tag::PLANAR_CONFIGURATION).unwrap(), 0);
        assert_eq!(out.get::<u16>(Tag::PIXEL_REPRESENTATION).unwrap(), 0);
        assert_eq!(frames_of(&out), vec![vec![10, 20, 30], vec![40, 20, 30]]);
        assert_eq!(
            tr.decode_frame(&source, 1)
                .unwrap()
                .data()
                .unwrap()
                .as_ref(),
            &[40, 20, 30]
        );
        assert!(tr.decode_frame(&source, 2).is_err());
        crate::pixel::codec::reset_codec_registry();
    }
    #[test]
    fn codec_metadata_survives_encoding_and_compressed_to_compressed() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(VariableRegistry));
        for source in [
            with_pixels(8, 2, 2, &[&[10, 20, 30, 40]]),
            variable_compressed(),
        ] {
            let tr = Transcoder::new(
                source.transfer_syntax(),
                syntaxes::RLE_LOSSLESS.clone(),
                None,
                None,
            );
            let out = tr.transcode_dataset(&source).unwrap();
            let expected_pi = if source.transfer_syntax().is_encapsulated() {
                "YBR_FULL"
            } else {
                "MONOCHROME1"
            };
            assert_eq!(
                out.get_str(PHOTOMETRIC_INTERPRETATION).unwrap(),
                expected_pi
            );
            if source.transfer_syntax().is_encapsulated() {
                assert_eq!(out.get::<u16>(Tag::SAMPLES_PER_PIXEL).unwrap(), 3);
                assert_eq!(out.get::<u16>(COLUMNS).unwrap(), 1);
                assert_eq!(frames_of(&out), vec![vec![10, 0], vec![40, 0]]);
            }
        }
        crate::pixel::codec::reset_codec_registry();
    }
    #[test]
    #[cfg(feature = "render")]
    fn public_rendering_uses_registered_decoder_and_its_rgb_metadata() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(VariableRegistry));
        let source = variable_compressed();
        let direct = crate::render::render_frame(&source, 1).unwrap();
        assert_eq!((direct.width(), direct.height()), (1, 1));
        assert_eq!(direct.as_bytes(), &[30, 20, 40, 255]);
        let image = crate::render::DicomImage::from_dataset_frame(&source, 1).unwrap();
        assert_eq!(
            (image.width(), image.height(), image.number_of_frames()),
            (1, 1, 2)
        );
        assert!(!image.is_grayscale());
        assert_eq!(image.render().unwrap().as_bytes(), direct.as_bytes());
        crate::pixel::codec::reset_codec_registry();
    }
    #[test]
    fn a_single_encapsulated_frame_rejects_out_of_range_indices() {
        let mut source = variable_compressed();
        source.insert_str(Tag::NUMBER_OF_FRAMES, Vr::Is, "1");
        let pixel = PixelDataRef::from_dataset(&source).unwrap();
        assert!(pixel.get_frame(1).is_err());
    }

    #[test]
    fn decoder_allocation_changes_refresh_native_vr_and_big_endian_bytes() {
        struct WideCodec;
        impl Codec for WideCodec {
            fn name(&self) -> &str {
                "wide allocation contract"
            }
            fn transfer_syntax(&self) -> &'static TransferSyntax {
                &syntaxes::JPEG_2000_LOSSLESS
            }
            fn encode(
                &self,
                _: &PixelDataRef<'_>,
                _: &mut PixelData<'_>,
                _: &CodecParams,
            ) -> Result<(), Error> {
                unreachable!()
            }
            fn decode(
                &self,
                src: &PixelDataRef<'_>,
                dst: &mut PixelData<'_>,
                _: &CodecParams,
            ) -> Result<(), Error> {
                assert!(src.transfer_syntax().is_encapsulated());
                let ds = dst.dataset_mut();
                ds.insert_value(BITS_ALLOCATED, 16u16);
                ds.insert_value(Tag::BITS_STORED, 16u16);
                ds.insert_value(Tag::HIGH_BIT, 15u16);
                if src.get_frame(0)?.data()?[0] == 1 {
                    dst.add_frame(0x1234u16.to_ne_bytes())?;
                } else {
                    dst.add_frame_buffer(Arc::new(MemoryBuffer::from(
                        0x1234u16.to_ne_bytes().to_vec(),
                    )))?;
                }
                Ok(())
            }
        }
        impl CodecRegistry for WideCodec {
            fn has_codec(&self, syntax: &TransferSyntax) -> bool {
                syntax.uid() == self.transfer_syntax().uid()
            }
            fn can_transcode(&self, _: &TransferSyntax, _: &TransferSyntax) -> bool {
                true
            }
            fn get_codec(&self, _: &TransferSyntax) -> Result<Arc<dyn Codec>, Error> {
                Ok(Arc::new(Self))
            }
        }
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(WideCodec));
        for marker in [1, 2] {
            let mut source = dataset(8, 1, 1);
            source.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
            source.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4");
            source.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
            PixelData::create_new(&mut source)
                .unwrap()
                .add_frame([marker, 0])
                .unwrap();
            let tr = Transcoder::new(
                source.transfer_syntax(),
                syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
                None,
                None,
            );
            let mut decoded = tr.transcode_dataset(&source).unwrap();
            assert_eq!(decoded.get_element(PIXEL_DATA).unwrap().vr(), Vr::Ow);
            decoded.set_transfer_syntax(syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone());
            let file = crate::io::file::DicomFile::from_dataset(decoded);
            let bytes = file.to_bytes(&Default::default()).unwrap();
            let reopened =
                crate::io::file::DicomFile::from_bytes(&bytes, &Default::default()).unwrap();
            assert_eq!(
                PixelDataRef::from_dataset(&reopened.dataset)
                    .unwrap()
                    .get_frame(0)
                    .unwrap()
                    .data()
                    .unwrap()
                    .as_ref(),
                &0x1234u16.to_ne_bytes()
            );
        }
        crate::pixel::codec::reset_codec_registry();
    }

    /// In-repo test-only codec: XOR every byte, so tests can prove the
    /// payload really moved through a codec.
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
        fn encode(
            &self,
            src: &PixelDataRef<'_>,
            dst: &mut PixelData<'_>,
            _p: &CodecParams,
        ) -> Result<(), Error> {
            for i in 0..src.number_of_frames() {
                let frame = src.get_frame(i)?;
                let bytes: Vec<u8> = frame.data().unwrap().iter().map(|b| b ^ 0xFF).collect();
                dst.add_frame(bytes)?;
            }
            Ok(())
        }
        fn decode(
            &self,
            src: &PixelDataRef<'_>,
            dst: &mut PixelData<'_>,
            _p: &CodecParams,
        ) -> Result<(), Error> {
            self.encode(src, dst, &CodecParams::None)
        }
    }

    struct MockRegistry {
        syntaxes: Vec<&'static TransferSyntax>,
    }

    impl CodecRegistry for MockRegistry {
        fn has_codec(&self, syntax: &TransferSyntax) -> bool {
            self.syntaxes.iter().any(|s| s.uid() == syntax.uid())
        }
        fn can_transcode(&self, from: &TransferSyntax, to: &TransferSyntax) -> bool {
            (!from.is_encapsulated() || self.has_codec(from))
                && (!to.is_encapsulated() || self.has_codec(to))
        }
        fn get_codec(&self, syntax: &TransferSyntax) -> Result<Arc<dyn Codec>, Error> {
            match self
                .syntaxes
                .iter()
                .copied()
                .find(|s| s.uid() == syntax.uid())
            {
                Some(entry) => Ok(Arc::new(MockCodec { syntax: entry })),
                None => Err(Error::Codec(CodecError::NotRegistered(
                    syntax.uid().to_string(),
                ))),
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

    fn with_pixels(bits: u16, w: u32, h: u32, frames: &[&[u8]]) -> Dataset {
        let mut ds = dataset(bits, w, h);
        ds.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        {
            let mut pd = PixelData::from_dataset(&mut ds).unwrap();
            for f in frames {
                pd.add_frame(f).unwrap();
            }
        }
        ds
    }

    fn frames_of(ds: &Dataset) -> Vec<Vec<u8>> {
        let mut owned = ds.clone();
        let pd = PixelData::from_dataset(&mut owned).unwrap();
        (0..pd.number_of_frames())
            .map(|i| pd.get_frame(i).unwrap().data().unwrap().to_vec())
            .collect()
    }

    #[test]
    fn missing_codec_fails_fast_naming_the_syntax() {
        let _guard = crate::pixel::codec::lock_registry();
        crate::pixel::codec::reset_codec_registry();
        let tr = Transcoder::new(
            syntaxes::JPEG_2000_LOSSLESS.clone(),
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            None,
            None,
        );
        match tr.transcode_dataset(&with_pixels(8, 2, 2, &[&[1, 2, 3, 4]])) {
            Err(Error::Codec(CodecError::NotRegistered(uid))) => {
                assert_eq!(uid, "1.2.840.10008.1.2.4.90")
            }
            Err(other) => panic!("expected NotRegistered, got {other}"),
            Ok(_) => panic!("expected NotRegistered, got success"),
        }
    }

    #[test]
    fn no_pixel_data_only_changes_the_syntax() {
        let tr = Transcoder::new(
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::JPEG_2000_LOSSLESS.clone(),
            None,
            None,
        );
        let out = tr.transcode_dataset(&dataset(8, 2, 2)).unwrap();
        assert_eq!(out.transfer_syntax().uid(), "1.2.840.10008.1.2.4.90");
        assert!(!out.contains(&PIXEL_DATA));
    }

    #[test]
    fn uncompressed_to_uncompressed_copies_frames_without_a_codec() {
        let _guard = crate::pixel::codec::lock_registry();
        crate::pixel::codec::reset_codec_registry();
        let ds = with_pixels(8, 2, 2, &[&[1, 2, 3, 4], &[5, 6, 7, 8]]);
        let tr = Transcoder::new(
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(),
            None,
            None,
        );
        let out = tr.transcode_dataset(&ds).unwrap();
        assert_eq!(
            frames_of(&out),
            vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]],
            "frames copied verbatim"
        );
        assert_eq!(out.transfer_syntax().uid(), "1.2.840.10008.1.2");
    }

    #[test]
    fn uncompressed_to_compressed_encodes() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry {
            syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS],
        }));
        let ds = with_pixels(8, 2, 2, &[&[1, 2, 3, 4]]);
        let tr = Transcoder::new(
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::JPEG_2000_LOSSLESS.clone(),
            None,
            None,
        );
        let out = tr.transcode_dataset(&ds).unwrap();
        assert_eq!(out.transfer_syntax().uid(), "1.2.840.10008.1.2.4.90");
        match out.item(&PIXEL_DATA) {
            Some(DicomItem::Fragments(f)) => {
                assert_eq!(f.vr(), Vr::Ob, "encapsulated data is stored as OB");
                assert_eq!(
                    f.fragments()[0].data().unwrap().as_ref(),
                    &[0xFE, 0xFD, 0xFC, 0xFB]
                );
            }
            other => panic!("expected fragments, got {other:?}"),
        }
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn compressed_to_uncompressed_decodes() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry {
            syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS],
        }));
        let mut ds = dataset(8, 2, 2);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        ds.insert_fragments(PIXEL_DATA, Vr::Ob)
            .push_fragment(Arc::new(MemoryBuffer::from(vec![0xFEu8, 0xFD, 0xFC, 0xFB])));
        let tr = Transcoder::new(
            syntaxes::JPEG_2000_LOSSLESS.clone(),
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            None,
            None,
        );
        let out = tr.transcode_dataset(&ds).unwrap();
        assert_eq!(
            frames_of(&out),
            vec![vec![1, 2, 3, 4]],
            "decode restores the payload"
        );
        assert_eq!(out.transfer_syntax().uid(), "1.2.840.10008.1.2.1");
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn compressed_to_compressed_round_trips_through_an_intermediate() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry {
            syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS, &syntaxes::RLE_LOSSLESS],
        }));
        let mut ds = dataset(8, 2, 2);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        ds.insert_fragments(PIXEL_DATA, Vr::Ob)
            .push_fragment(Arc::new(MemoryBuffer::from(vec![0xFEu8, 0xFD, 0xFC, 0xFB])));
        let tr = Transcoder::new(
            syntaxes::JPEG_2000_LOSSLESS.clone(),
            syntaxes::RLE_LOSSLESS.clone(),
            None,
            None,
        );
        let out = tr.transcode_dataset(&ds).unwrap();
        assert_eq!(out.transfer_syntax().uid(), "1.2.840.10008.1.2.5");
        // decode(0xFE..) = [1,2,3,4], then encode = [0xFE..] again
        match out.item(&PIXEL_DATA) {
            Some(DicomItem::Fragments(f)) => {
                assert_eq!(
                    f.fragments()[0].data().unwrap().as_ref(),
                    &[0xFE, 0xFD, 0xFC, 0xFB]
                );
            }
            other => panic!("expected fragments, got {other:?}"),
        }
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn source_dataset_is_never_mutated() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry {
            syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS],
        }));
        let ds = with_pixels(8, 2, 2, &[&[1, 2, 3, 4]]);
        let before = ds.clone();
        let tr = Transcoder::new(
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::JPEG_2000_LOSSLESS.clone(),
            None,
            None,
        );
        let _ = tr.transcode_dataset(&ds).unwrap();
        assert_eq!(ds, before, "transcode must not touch its input");
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn decode_frame_isolates_one_frame() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry {
            syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS],
        }));
        let mut ds = dataset(8, 2, 2);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        ds.insert_str(Tag::NUMBER_OF_FRAMES, Vr::Is, "2");
        {
            let frags = ds.insert_fragments(PIXEL_DATA, Vr::Ob);
            frags.push_fragment(Arc::new(MemoryBuffer::from(vec![0xFEu8, 0xFD, 0xFC, 0xFB])));
            frags.push_fragment(Arc::new(MemoryBuffer::from(vec![0xFEu8, 0xFD, 0xFC, 0xFB])));
        }
        let before = ds.clone();
        let tr = Transcoder::new(
            syntaxes::JPEG_2000_LOSSLESS.clone(),
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            None,
            None,
        );
        let frame = tr.decode_frame(&ds, 0).unwrap();
        assert_eq!(frame.data().unwrap().as_ref(), &[1, 2, 3, 4]);
        assert_eq!(ds, before, "TR-02: the source stays untouched");
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn lossy_bookkeeping_appends_per_generation() {
        let mut ds = dataset(8, 2, 2);
        Transcoder::mark_lossy(&mut ds, "ISO_10918_1", 1000, 200);
        assert_eq!(ds.get_str(LOSSY_IMAGE_COMPRESSION).unwrap(), "01");
        assert_eq!(
            ds.get_str(LOSSY_IMAGE_COMPRESSION_METHOD).unwrap(),
            "ISO_10918_1"
        );
        assert_eq!(ds.get_str(LOSSY_IMAGE_COMPRESSION_RATIO).unwrap(), "5.000");

        Transcoder::mark_lossy(&mut ds, "ISO_15444_1", 1000, 400);
        assert_eq!(
            ds.get_str(LOSSY_IMAGE_COMPRESSION_METHOD).unwrap(),
            "ISO_10918_1\\ISO_15444_1"
        );
        assert_eq!(
            ds.get_str(LOSSY_IMAGE_COMPRESSION_RATIO).unwrap(),
            "5.000\\2.500"
        );
    }

    #[test]
    fn ratio_uses_three_decimals_and_handles_zero() {
        let mut ds = dataset(8, 2, 2);
        Transcoder::mark_lossy(&mut ds, "X", 1, 3);
        assert_eq!(ds.get_str(LOSSY_IMAGE_COMPRESSION_RATIO).unwrap(), "0.333");
        let mut ds = dataset(8, 2, 2);
        Transcoder::mark_lossy(&mut ds, "X", 100, 0);
        assert_eq!(
            ds.get_str(LOSSY_IMAGE_COMPRESSION_RATIO).unwrap(),
            "0.000",
            "no division by zero"
        );
    }

    #[test]
    fn transcoding_to_a_lossy_syntax_marks_the_output() {
        // verify the flag on the transcoded dataset, not just that a
        // helper exists. JPEG Baseline is ISO_10918_1 lossy.
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry {
            syntaxes: vec![&syntaxes::JPEG_PROCESS_1],
        }));
        let ds = with_pixels(8, 2, 2, &[&[1, 2, 3, 4]]);
        let tr = Transcoder::new(
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::JPEG_PROCESS_1.clone(),
            None,
            None,
        );
        let out = tr.transcode_dataset(&ds).unwrap();
        assert_eq!(
            out.get_str(LOSSY_IMAGE_COMPRESSION).unwrap(),
            "01",
            "output flagged lossy"
        );
        assert_eq!(
            out.get_str(LOSSY_IMAGE_COMPRESSION_METHOD).unwrap(),
            "ISO_10918_1"
        );
        // 4 source bytes → 4 encoded bytes (the mock XOR keeps length): ratio 1.000
        assert_eq!(out.get_str(LOSSY_IMAGE_COMPRESSION_RATIO).unwrap(), "1.000");
        assert!(
            ds.get_str(LOSSY_IMAGE_COMPRESSION).is_err(),
            "the source is not mutated"
        );
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn transcode_extracts_embedded_overlays() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry {
            syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS],
        }));
        let mut ds = dataset(8, 8, 1);
        ds.insert_value(crate::pixel::BITS_STORED, 7u16);
        ds.insert_value(Tag::new(0x6000, 0x0010), 1u16);
        ds.insert_value(Tag::new(0x6000, 0x0011), 8u16);
        ds.insert_str(Tag::new(0x6000, 0x0040), Vr::Cs, "G");
        ds.insert_value(Tag::new(0x6000, 0x0100), 1u16);
        ds.insert_value(Tag::new(0x6000, 0x0102), 7u16);
        ds.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds.insert_or_update(DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                PIXEL_DATA,
                Vr::Ob,
                Arc::new(MemoryBuffer::from(vec![0x80u8, 0, 0, 0, 0, 0, 0, 0])),
            ),
        ));
        let tr = Transcoder::new(
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::JPEG_2000_LOSSLESS.clone(),
            None,
            None,
        );
        let out = tr.transcode_dataset(&ds).unwrap();
        assert!(
            out.contains(&Tag::new(0x6000, 0x3000)),
            "overlay extracted into a standalone element"
        );
        assert_eq!(
            out.get_or(Tag::new(0x6000, 0x0102), 99u16),
            0,
            "bit position normalized"
        );
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn require_codec_reports_missing_registrations() {
        let _guard = crate::pixel::codec::lock_registry();
        crate::pixel::codec::reset_codec_registry();
        assert!(
            require_codec(&syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).is_ok(),
            "uncompressed needs no codec"
        );
        assert!(require_codec(&syntaxes::JPEG_2000_LOSSLESS).is_err());
    }

    #[test]
    fn transcode_file_updates_the_meta_group() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry {
            syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS],
        }));
        let mut file =
            crate::io::file::DicomFile::from_dataset(with_pixels(8, 2, 2, &[&[1, 2, 3, 4]]));
        file.file_meta
            .insert_str(Tag::TRANSFER_SYNTAX_UID, Vr::Ui, "1.2.840.10008.1.2.1");
        let tr = Transcoder::new(
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::JPEG_2000_LOSSLESS.clone(),
            None,
            None,
        );
        let out = tr.transcode_file(&file).unwrap();
        assert_eq!(
            out.dataset.transfer_syntax().uid(),
            "1.2.840.10008.1.2.4.90"
        );
        assert_eq!(
            out.file_meta.get_str(Tag::TRANSFER_SYNTAX_UID).unwrap(),
            "1.2.840.10008.1.2.4.90",
            "meta group transfer syntax follows the dataset"
        );
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn dataset_clone_with_syntax_transcodes_without_touching_the_source() {
        let ds = with_pixels(8, 2, 2, &[&[1, 2, 3, 4]]);
        let out = ds
            .clone_with_syntax(syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(), None)
            .unwrap();
        assert_eq!(out.transfer_syntax().uid(), "1.2.840.10008.1.2");
        assert_eq!(
            frames_of(&out),
            vec![vec![1, 2, 3, 4]],
            "frames are copied through"
        );
        assert_eq!(
            ds.transfer_syntax().uid(),
            "1.2.840.10008.1.2.1",
            "the source syntax is untouched"
        );
    }

    #[test]
    fn file_clone_with_syntax_updates_the_meta_group() {
        let mut file =
            crate::io::file::DicomFile::from_dataset(with_pixels(8, 2, 2, &[&[1, 2, 3, 4]]));
        file.file_meta
            .insert_str(Tag::TRANSFER_SYNTAX_UID, Vr::Ui, "1.2.840.10008.1.2.1");
        let out = file
            .clone_with_syntax(syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(), None)
            .unwrap();
        assert_eq!(out.dataset.transfer_syntax().uid(), "1.2.840.10008.1.2");
        assert_eq!(
            out.file_meta.get_str(Tag::TRANSFER_SYNTAX_UID).unwrap(),
            "1.2.840.10008.1.2"
        );
    }
}
