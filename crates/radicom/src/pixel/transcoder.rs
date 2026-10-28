//! Transcoder (TR-01..03; docs/06-pixel-codec.md §5).
//!
//! Decides how to move pixel data between transfer syntaxes, resolves codecs
//! lazily through the global registry, and keeps the lossy-compression
//! bookkeeping correct. Codecs themselves never live here.

use crate::error::{CodecError, Error};
use crate::model::dataset::Dataset;
use crate::model::item::{DicomFragmentSequence, DicomItem};
use crate::model::tag::Tag;
use crate::model::transfer_syntax::TransferSyntax;
use crate::model::vr::Vr;
use crate::pixel::codec::{codec_registry, CodecParams, DirectTranscoder};
use crate::pixel::overlay::extract_overlays;
use crate::pixel::{PixelData, PIXEL_DATA};
use std::sync::Arc;

/// `(0028,2110)` Lossy Image Compression.
pub const LOSSY_IMAGE_COMPRESSION: Tag = Tag::new(0x0028, 0x2110);
/// `(0028,2112)` Lossy Image Compression Ratio.
pub const LOSSY_IMAGE_COMPRESSION_RATIO: Tag = Tag::new(0x0028, 0x2112);
/// `(0028,2114)` Lossy Image Compression Method.
pub const LOSSY_IMAGE_COMPRESSION_METHOD: Tag = Tag::new(0x0028, 0x2114);

/// Moves pixel data from one transfer syntax to another (TR-01).
#[derive(Debug, Clone)]
pub struct Transcoder {
    from: Arc<TransferSyntax>,
    to: Arc<TransferSyntax>,
    in_params: CodecParams,
    out_params: CodecParams,
}

impl Transcoder {
    /// Builds a transcoder; codecs are resolved lazily at use time (§5.1).
    pub fn new(
        from: Arc<TransferSyntax>,
        to: Arc<TransferSyntax>,
        in_params: Option<CodecParams>,
        out_params: Option<CodecParams>,
    ) -> Self {
        Transcoder {
            from,
            to,
            in_params: in_params.unwrap_or(CodecParams::None),
            out_params: out_params.unwrap_or(CodecParams::None),
        }
    }

    pub fn from_syntax(&self) -> &TransferSyntax {
        &self.from
    }

    pub fn to_syntax(&self) -> &TransferSyntax {
        &self.to
    }

    /// Transcodes a dataset, returning a new one (§5.2 decision table).
    ///
    /// The input is never mutated: every path works on a clone.
    pub fn transcode_dataset(&self, ds: &Dataset) -> Result<Dataset, Error> {
        let mut out = ds.clone();
        // row 1: no pixel data — only the syntax changes
        if !ds.contains(&PIXEL_DATA) {
            out.set_transfer_syntax(self.to.clone());
            return Ok(out);
        }
        // embedded overlays must be extracted while the pixel data is still
        // uncompressed, before `out` claims the target syntax (§5.4)
        extract_overlays(&mut out)?;
        out.set_transfer_syntax(self.to.clone());

        let uncompressed_from = !self.from.is_encapsulated();
        let uncompressed_to = !self.to.is_encapsulated();

        let frames = if uncompressed_from && uncompressed_to {
            // row 2: frame-wise copy, no codec involved
            self.read_frames(ds)?
        } else if !uncompressed_from && uncompressed_to {
            // row 3: decode only
            self.decode_frames(ds)?
        } else if uncompressed_from && !uncompressed_to {
            // row 4: encode only
            self.encode_frames(ds)?
        } else {
            // row 5: a direct hook wins, otherwise decode then encode
            match codec_registry().get_direct_transcoder(&self.from, &self.to) {
                Some(hook) => self.run_direct(ds, hook)?,
                None => {
                    let intermediate = self.decode_to_uncompressed(ds)?;
                    self.encode_frames(&intermediate)?
                }
            }
        };

        out.remove(PIXEL_DATA);
        out.remove(Tag::new(0x0028, 0x0008));
        {
            let mut pd = PixelData::from_dataset(&mut out)?;
            for frame in frames {
                pd.add_frame(frame)?;
            }
        }
        self.finish(&mut out)?;
        Ok(out)
    }

    /// Convenience: transcode and clone a file (TR-03).
    pub fn transcode_file(&self, file: &crate::io::file::DicomFile) -> Result<crate::io::file::DicomFile, Error> {
        let dataset = self.transcode_dataset(&file.dataset)?;
        let mut out = file.clone();
        out.dataset = dataset;
        // the meta group's transfer syntax is refreshed on save
        out.file_meta.insert_str(Tag::new(0x0002, 0x0010), Vr::Ui, self.to.uid());
        Ok(out)
    }

    /// Decodes one frame without mutating the source (TR-02, §5.6).
    pub fn decode_frame(&self, ds: &Dataset, frame: u32) -> Result<crate::model::buffer::SharedBuffer, Error> {
        if !self.from.is_encapsulated() {
            let mut owned = ds.clone();
            return PixelData::from_dataset(&mut owned)?.get_frame(frame);
        }
        let codec = codec_registry().get_codec(&self.from)?;
        let bytes = self.read_frame(ds, frame)?;

        // decode into a scratch dataset holding just this frame
        let mut target = ds.clone();
        target.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        target.remove(PIXEL_DATA);
        target.remove(Tag::new(0x0028, 0x0008));
        {
            let mut dst = PixelData::from_dataset(&mut target)?;
            dst.add_frame(&bytes)?;
        }
        let mut decoded = target.clone();
        decoded.remove(PIXEL_DATA);
        {
            let src = PixelData::from_dataset(&mut target)?;
            let mut dst = PixelData::from_dataset(&mut decoded)?;
            codec.decode(&src, &mut dst, &self.in_params)?;
        }
        PixelData::from_dataset(&mut decoded)?.get_frame(0)
    }

    // -- flows ---------------------------------------------------------------

    /// Every frame of `ds` as owned bytes, in order.
    fn read_frames(&self, ds: &Dataset) -> Result<Vec<Vec<u8>>, Error> {
        let mut owned = ds.clone();
        let pd = PixelData::from_dataset(&mut owned)?;
        let count = pd.number_of_frames();
        let mut frames = Vec::with_capacity(count as usize);
        for i in 0..count {
            frames.push(pd.get_frame(i)?.data().to_vec());
        }
        Ok(frames)
    }

    /// One frame of `ds` as owned bytes.
    fn read_frame(&self, ds: &Dataset, frame: u32) -> Result<Vec<u8>, Error> {
        let mut owned = ds.clone();
        Ok(PixelData::from_dataset(&mut owned)?.get_frame(frame)?.data().to_vec())
    }

    /// Row 3: decode `ds` and return the decoded frames.
    fn decode_frames(&self, ds: &Dataset) -> Result<Vec<Vec<u8>>, Error> {
        let codec = codec_registry().get_codec(&self.from)?;
        let frames = self.read_frames(ds)?;
        let mut source = ds.clone();
        source.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        source.remove(PIXEL_DATA);
        {
            let mut pd = PixelData::from_dataset(&mut source)?;
            for frame in frames {
                pd.add_frame(frame)?;
            }
        }
        let mut decoded = source.clone();
        decoded.remove(PIXEL_DATA);
        {
            let src = PixelData::from_dataset(&mut source)?;
            let mut dst = PixelData::from_dataset(&mut decoded)?;
            codec.decode(&src, &mut dst, &self.in_params)?;
        }
        self.read_frames(&decoded)
    }

    /// Row 4: encode `ds` and return the encoded frames.
    fn encode_frames(&self, ds: &Dataset) -> Result<Vec<Vec<u8>>, Error> {
        let codec = codec_registry().get_codec(&self.to)?;
        let mut encoded = ds.clone();
        encoded.set_transfer_syntax(self.to.clone());
        encoded.remove(PIXEL_DATA);
        {
            let mut source = ds.clone();
            let src = PixelData::from_dataset(&mut source)?;
            let mut dst = PixelData::from_dataset(&mut encoded)?;
            codec.encode(&src, &mut dst, &self.out_params)?;
        }
        self.read_frames(&encoded)
    }

    /// Row 5 via the direct hook.
    fn run_direct(&self, ds: &Dataset, hook: Arc<dyn DirectTranscoder>) -> Result<Vec<Vec<u8>>, Error> {
        let mut encoded = ds.clone();
        encoded.set_transfer_syntax(self.to.clone());
        encoded.remove(PIXEL_DATA);
        {
            let mut source = ds.clone();
            let src = PixelData::from_dataset(&mut source)?;
            let mut dst = PixelData::from_dataset(&mut encoded)?;
            hook.transcode(&src, &mut dst)?;
        }
        self.read_frames(&encoded)
    }

    /// Row 5 fallback: decode to explicit VR little endian first.
    fn decode_to_uncompressed(&self, ds: &Dataset) -> Result<Dataset, Error> {
        let frames = self.decode_frames(ds)?;
        let mut out = ds.clone();
        out.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        out.remove(PIXEL_DATA);
        out.remove(Tag::new(0x0028, 0x0008));
        {
            let mut pd = PixelData::from_dataset(&mut out)?;
            for frame in frames {
                pd.add_frame(frame)?;
            }
        }
        Ok(out)
    }

    /// §5.3 bookkeeping: encapsulated VR semantics and overlay extraction.
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

    /// Records lossy compression on `out` (§5.3), given the original and
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

/// Ensures the output syntax has a codec, failing fast otherwise (§3).
pub fn require_codec(syntax: &TransferSyntax) -> Result<(), Error> {
    if syntax.is_encapsulated() && !codec_registry().has_codec(syntax) {
        return Err(Error::Codec(CodecError::NotRegistered(syntax.uid().to_string())));
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
        Transcoder::new(self.dataset.transfer_syntax(), syntax, None, out_params).transcode_file(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::buffer::MemoryBuffer;
    use crate::model::transfer_syntax::syntaxes;
    use crate::pixel::codec::{set_codec_registry, Codec, CodecRegistry};
    use crate::pixel::{BITS_ALLOCATED, COLUMNS, PHOTOMETRIC_INTERPRETATION, ROWS};

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
        fn encode(&self, src: &PixelData<'_>, dst: &mut PixelData<'_>, _p: &CodecParams) -> Result<(), Error> {
            for i in 0..src.number_of_frames() {
                let frame = src.get_frame(i)?;
                let bytes: Vec<u8> = frame.data().iter().map(|b| b ^ 0xFF).collect();
                dst.add_frame(bytes)?;
            }
            Ok(())
        }
        fn decode(&self, src: &PixelData<'_>, dst: &mut PixelData<'_>, _p: &CodecParams) -> Result<(), Error> {
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
            (!from.is_encapsulated() || self.has_codec(from)) && (!to.is_encapsulated() || self.has_codec(to))
        }
        fn get_codec(&self, syntax: &TransferSyntax) -> Result<Arc<dyn Codec>, Error> {
            match self.syntaxes.iter().copied().find(|s| s.uid() == syntax.uid()) {
                Some(entry) => Ok(Arc::new(MockCodec { syntax: entry })),
                None => Err(Error::Codec(CodecError::NotRegistered(syntax.uid().to_string()))),
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
            .map(|i| pd.get_frame(i).unwrap().data().to_vec())
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
            Err(Error::Codec(CodecError::NotRegistered(uid))) => assert_eq!(uid, "1.2.840.10008.1.2.4.90"),
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
        assert_eq!(frames_of(&out), vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]], "frames copied verbatim");
        assert_eq!(out.transfer_syntax().uid(), "1.2.840.10008.1.2");
    }

    #[test]
    fn uncompressed_to_compressed_encodes() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry { syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS] }));
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
                assert_eq!(f.fragments()[0].data().as_ref(), &[0xFE, 0xFD, 0xFC, 0xFB]);
            }
            other => panic!("expected fragments, got {other:?}"),
        }
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn compressed_to_uncompressed_decodes() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry { syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS] }));
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
        assert_eq!(frames_of(&out), vec![vec![1, 2, 3, 4]], "decode restores the payload");
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
                assert_eq!(f.fragments()[0].data().as_ref(), &[0xFE, 0xFD, 0xFC, 0xFB]);
            }
            other => panic!("expected fragments, got {other:?}"),
        }
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn source_dataset_is_never_mutated() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry { syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS] }));
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
        set_codec_registry(Arc::new(MockRegistry { syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS] }));
        let mut ds = dataset(8, 2, 2);
        ds.set_transfer_syntax(syntaxes::JPEG_2000_LOSSLESS.clone());
        ds.insert_str(Tag::new(0x0028, 0x0008), Vr::Is, "2");
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
        assert_eq!(frame.data().as_ref(), &[1, 2, 3, 4]);
        assert_eq!(ds, before, "TR-02: the source stays untouched");
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn lossy_bookkeeping_appends_per_generation() {
        let mut ds = dataset(8, 2, 2);
        Transcoder::mark_lossy(&mut ds, "ISO_10918_1", 1000, 200);
        assert_eq!(ds.get_str(LOSSY_IMAGE_COMPRESSION).unwrap(), "01");
        assert_eq!(ds.get_str(LOSSY_IMAGE_COMPRESSION_METHOD).unwrap(), "ISO_10918_1");
        assert_eq!(ds.get_str(LOSSY_IMAGE_COMPRESSION_RATIO).unwrap(), "5.000");

        Transcoder::mark_lossy(&mut ds, "ISO_15444_1", 1000, 400);
        assert_eq!(ds.get_str(LOSSY_IMAGE_COMPRESSION_METHOD).unwrap(), "ISO_10918_1\\ISO_15444_1");
        assert_eq!(ds.get_str(LOSSY_IMAGE_COMPRESSION_RATIO).unwrap(), "5.000\\2.500");
    }

    #[test]
    fn ratio_uses_three_decimals_and_handles_zero() {
        let mut ds = dataset(8, 2, 2);
        Transcoder::mark_lossy(&mut ds, "X", 1, 3);
        assert_eq!(ds.get_str(LOSSY_IMAGE_COMPRESSION_RATIO).unwrap(), "0.333");
        let mut ds = dataset(8, 2, 2);
        Transcoder::mark_lossy(&mut ds, "X", 100, 0);
        assert_eq!(ds.get_str(LOSSY_IMAGE_COMPRESSION_RATIO).unwrap(), "0.000", "no division by zero");
    }

    #[test]
    fn transcode_extracts_embedded_overlays() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry { syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS] }));
        let mut ds = dataset(8, 8, 1);
        ds.insert_value(crate::pixel::BITS_STORED, 7u16);
        ds.insert_value(Tag::new(0x6000, 0x0010), 1u16);
        ds.insert_value(Tag::new(0x6000, 0x0011), 8u16);
        ds.insert_str(Tag::new(0x6000, 0x0040), Vr::Cs, "G");
        ds.insert_value(Tag::new(0x6000, 0x0100), 1u16);
        ds.insert_value(Tag::new(0x6000, 0x0102), 7u16);
        ds.set_transfer_syntax(syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds.insert_or_update(DicomItem::Element(crate::model::element::DicomElement::new_bytes(
            PIXEL_DATA,
            Vr::Ob,
            Arc::new(MemoryBuffer::from(vec![0x80u8, 0, 0, 0, 0, 0, 0, 0])),
        )));
        let tr = Transcoder::new(
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::JPEG_2000_LOSSLESS.clone(),
            None,
            None,
        );
        let out = tr.transcode_dataset(&ds).unwrap();
        assert!(out.contains(&Tag::new(0x6000, 0x3000)), "overlay extracted into a standalone element");
        assert_eq!(out.get_or(Tag::new(0x6000, 0x0102), 99u16), 0, "bit position normalized");
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn require_codec_reports_missing_registrations() {
        let _guard = crate::pixel::codec::lock_registry();
        crate::pixel::codec::reset_codec_registry();
        assert!(require_codec(&syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).is_ok(), "uncompressed needs no codec");
        assert!(require_codec(&syntaxes::JPEG_2000_LOSSLESS).is_err());
    }

    #[test]
    fn transcode_file_updates_the_meta_group() {
        let _guard = crate::pixel::codec::lock_registry();
        set_codec_registry(Arc::new(MockRegistry { syntaxes: vec![&syntaxes::JPEG_2000_LOSSLESS] }));
        let mut file = crate::io::file::DicomFile::from_dataset(with_pixels(8, 2, 2, &[&[1, 2, 3, 4]]));
        file.file_meta.insert_str(Tag::new(0x0002, 0x0010), Vr::Ui, "1.2.840.10008.1.2.1");
        let tr = Transcoder::new(
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::JPEG_2000_LOSSLESS.clone(),
            None,
            None,
        );
        let out = tr.transcode_file(&file).unwrap();
        assert_eq!(out.dataset.transfer_syntax().uid(), "1.2.840.10008.1.2.4.90");
        assert_eq!(
            out.file_meta.get_str(Tag::new(0x0002, 0x0010)).unwrap(),
            "1.2.840.10008.1.2.4.90",
            "meta group transfer syntax follows the dataset"
        );
        crate::pixel::codec::reset_codec_registry();
    }

    #[test]
    fn dataset_clone_with_syntax_transcodes_without_touching_the_source() {
        let ds = with_pixels(8, 2, 2, &[&[1, 2, 3, 4]]);
        let out = ds.clone_with_syntax(syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(), None).unwrap();
        assert_eq!(out.transfer_syntax().uid(), "1.2.840.10008.1.2");
        assert_eq!(frames_of(&out), vec![vec![1, 2, 3, 4]], "frames are copied through");
        assert_eq!(ds.transfer_syntax().uid(), "1.2.840.10008.1.2.1", "the source syntax is untouched");
    }

    #[test]
    fn file_clone_with_syntax_updates_the_meta_group() {
        let mut file = crate::io::file::DicomFile::from_dataset(with_pixels(8, 2, 2, &[&[1, 2, 3, 4]]));
        file.file_meta.insert_str(Tag::new(0x0002, 0x0010), Vr::Ui, "1.2.840.10008.1.2.1");
        let out = file.clone_with_syntax(syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(), None).unwrap();
        assert_eq!(out.dataset.transfer_syntax().uid(), "1.2.840.10008.1.2");
        assert_eq!(out.file_meta.get_str(Tag::new(0x0002, 0x0010)).unwrap(), "1.2.840.10008.1.2");
    }
}
