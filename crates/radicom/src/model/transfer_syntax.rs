//! Transfer syntax registry (TS-01, TS-02, TS-03; docs/01-architecture.md §2).

use crate::error::{DataError, Error};
use crate::model::endian::Endian;
use crate::model::uid::{Uid, UidType};
use crate::uids;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// A DICOM transfer syntax: the UID plus its encoding attributes.
#[derive(Debug, Clone)]
pub struct TransferSyntax {
    uid: Arc<str>,
    name: Arc<str>,
    uid_type: UidType,
    is_retired: bool,
    is_explicit_vr: bool,
    is_encapsulated: bool,
    is_lossy: bool,
    lossy_method: Option<&'static str>,
    is_deflate: bool,
    endian: Endian,
    swap_pixel_data: bool,
}

impl TransferSyntax {
    fn new(uid: &'static str, name: &'static str) -> Self {
        TransferSyntax {
            uid: Arc::from(uid),
            name: Arc::from(name),
            uid_type: UidType::TransferSyntax,
            is_retired: false,
            is_explicit_vr: true,
            is_encapsulated: false,
            is_lossy: false,
            lossy_method: None,
            is_deflate: false,
            endian: Endian::Little,
            swap_pixel_data: false,
        }
    }

    fn retired(mut self) -> Self {
        self.is_retired = true;
        self
    }

    fn implicit(mut self) -> Self {
        self.is_explicit_vr = false;
        self
    }

    fn encapsulated(mut self) -> Self {
        self.is_encapsulated = true;
        self
    }

    fn lossy(mut self, method: &'static str) -> Self {
        self.is_lossy = true;
        self.lossy_method = Some(method);
        self
    }

    fn deflate(mut self) -> Self {
        self.is_deflate = true;
        self
    }

    fn big(mut self) -> Self {
        self.endian = Endian::Big;
        self
    }

    fn with_swap_pixel_data(mut self) -> Self {
        self.swap_pixel_data = true;
        self
    }

    // -- accessors ---------------------------------------------------------

    pub fn uid(&self) -> &str {
        &self.uid
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uid_type(&self) -> UidType {
        self.uid_type
    }

    pub fn is_retired(&self) -> bool {
        self.is_retired
    }

    pub fn is_explicit_vr(&self) -> bool {
        self.is_explicit_vr
    }

    /// Pixel data is stored as fragment sequences (compressed syntaxes).
    pub fn is_encapsulated(&self) -> bool {
        self.is_encapsulated
    }

    pub fn is_lossy(&self) -> bool {
        self.is_lossy
    }

    pub fn lossy_compression_method(&self) -> Option<&'static str> {
        self.lossy_method
    }

    pub fn is_deflate(&self) -> bool {
        self.is_deflate
    }

    pub fn endian(&self) -> Endian {
        self.endian
    }

    /// GE private big-endian export quirk: the dataset is little-endian but
    /// pixel data words need swapping.
    pub fn swap_pixel_data(&self) -> bool {
        self.swap_pixel_data
    }
}

impl PartialEq for TransferSyntax {
    fn eq(&self, other: &Self) -> bool {
        self.uid == other.uid
    }
}

impl Eq for TransferSyntax {}

impl std::fmt::Display for TransferSyntax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name, self.uid)
    }
}

// -- built-in registry (TS-01) --------------------------------------------

/// All built-in entries, in registry order. Standard UID keys come from the
/// generated dictionary; encoding flags follow fo-dicom's transfer syntax table.
pub fn builtin_entries() -> Vec<TransferSyntax> {
    vec![
        TransferSyntax::new(
            "1.2.840.113619.5.2",
            "Private GE Implicit VR Big Endian",
        )
        .implicit()
        .with_swap_pixel_data(),
        TransferSyntax::new(
            uids::IMPLICIT_VR_LITTLE_ENDIAN,
            "Implicit VR Little Endian: Default Transfer Syntax for DICOM",
        )
        .implicit(),
        TransferSyntax::new(
            uids::EXPLICIT_VR_LITTLE_ENDIAN,
            "Explicit VR Little Endian",
        ),
        TransferSyntax::new(
            uids::EXPLICIT_VR_BIG_ENDIAN_RETIRED,
            "Explicit VR Big Endian (Retired)",
        )
        .retired()
        .big(),
        TransferSyntax::new(
            uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN,
            "Deflated Explicit VR Little Endian",
        )
        .deflate(),
        TransferSyntax::new(
            uids::JPEG_BASELINE8_BIT,
            "JPEG Baseline (Process 1): Default Transfer Syntax for Lossy JPEG 8 Bit Image Compression",
        )
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_EXTENDED12_BIT,
            "JPEG Extended (Process 2 & 4): Default Transfer Syntax for Lossy JPEG 12 Bit Image Compression (Process 4 only)",
        )
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_EXTENDED35_RETIRED,
            "JPEG Extended (Process 3 & 5) (Retired)",
        )
        .retired()
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_SPECTRAL_SELECTION_NON_HIERARCHICAL68_RETIRED,
            "JPEG Spectral Selection, Non-Hierarchical (Process 6 & 8) (Retired)",
        )
        .retired()
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_SPECTRAL_SELECTION_NON_HIERARCHICAL79_RETIRED,
            "JPEG Spectral Selection, Non-Hierarchical (Process 7 & 9) (Retired)",
        )
        .retired()
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_FULL_PROGRESSION_NON_HIERARCHICAL1012_RETIRED,
            "JPEG Full Progression, Non-Hierarchical (Process 10 & 12) (Retired)",
        )
        .retired()
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_FULL_PROGRESSION_NON_HIERARCHICAL1113_RETIRED,
            "JPEG Full Progression, Non-Hierarchical (Process 11 & 13) (Retired)",
        )
        .retired()
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_LOSSLESS,
            "JPEG Lossless, Non-Hierarchical (Process 14)",
        )
        .encapsulated(),
        TransferSyntax::new(
            uids::JPEG_LOSSLESS_NON_HIERARCHICAL15_RETIRED,
            "JPEG Lossless, Non-Hierarchical (Process 15) (Retired)",
        )
        .retired()
        .encapsulated(),
        TransferSyntax::new(
            uids::JPEG_EXTENDED_HIERARCHICAL1618_RETIRED,
            "JPEG Extended, Hierarchical (Process 16 & 18) (Retired)",
        )
        .retired()
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_EXTENDED_HIERARCHICAL1719_RETIRED,
            "JPEG Extended, Hierarchical (Process 17 & 19) (Retired)",
        )
        .retired()
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_SPECTRAL_SELECTION_HIERARCHICAL2022_RETIRED,
            "JPEG Spectral Selection, Hierarchical (Process 20 & 22) (Retired)",
        )
        .retired()
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_SPECTRAL_SELECTION_HIERARCHICAL2123_RETIRED,
            "JPEG Spectral Selection, Hierarchical (Process 21 & 23) (Retired)",
        )
        .retired()
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_FULL_PROGRESSION_HIERARCHICAL2426_RETIRED,
            "JPEG Full Progression, Hierarchical (Process 24 & 26) (Retired)",
        )
        .retired()
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_FULL_PROGRESSION_HIERARCHICAL2527_RETIRED,
            "JPEG Full Progression, Hierarchical (Process 25 & 27) (Retired)",
        )
        .retired()
        .encapsulated()
        .lossy("ISO_10918_1"),
        TransferSyntax::new(
            uids::JPEG_LOSSLESS_HIERARCHICAL28_RETIRED,
            "JPEG Lossless, Hierarchical (Process 28) (Retired)",
        )
        .retired()
        .encapsulated(),
        TransferSyntax::new(
            uids::JPEG_LOSSLESS_HIERARCHICAL29_RETIRED,
            "JPEG Lossless, Hierarchical (Process 29) (Retired)",
        )
        .retired()
        .encapsulated(),
        TransferSyntax::new(
            uids::JPEG_LOSSLESS_SV1,
            "JPEG Lossless, Non-Hierarchical, First-Order Prediction (Process 14 [Selection Value 1]): Default Transfer Syntax for Lossless JPEG Image Compression",
        )
        .encapsulated(),
        TransferSyntax::new(
            uids::JPEGLS_LOSSLESS,
            "JPEG-LS Lossless Image Compression",
        )
        .encapsulated(),
        TransferSyntax::new(
            uids::JPEGLS_NEAR_LOSSLESS,
            "JPEG-LS Lossy (Near-Lossless) Image Compression",
        )
        .encapsulated()
        .lossy("ISO_14495_1"),
        TransferSyntax::new(
            uids::JPEG2000_LOSSLESS,
            "JPEG 2000 Image Compression (Lossless Only)",
        )
        .encapsulated(),
        TransferSyntax::new(
            uids::JPEG2000,
            "JPEG 2000 Image Compression",
        )
        .encapsulated()
        .lossy("ISO_15444_1"),
        TransferSyntax::new(
            uids::JPEG2000_MC_LOSSLESS,
            "JPEG 2000 Part 2 Multi-component Image Compression (Lossless Only)",
        )
        .encapsulated(),
        TransferSyntax::new(
            uids::JPEG2000_MC,
            "JPEG 2000 Part 2 Multi-component Image Compression",
        )
        .encapsulated()
        .lossy("ISO_15444_2"),
        TransferSyntax::new(
            uids::JPIP_REFERENCED,
            "JPIP Referenced",
        ),
        TransferSyntax::new(
            uids::JPIP_REFERENCED_DEFLATE,
            "JPIP Referenced Deflate",
        )
        .deflate(),
        TransferSyntax::new(
            uids::MPEG2_MPML,
            "MPEG2 Main Profile / Main Level",
        )
        .encapsulated()
        .lossy("ISO_13818_2"),
        TransferSyntax::new(
            uids::MPEG2_MPMLF,
            "Fragmentable MPEG2 Main Profile / Main Level",
        )
        .encapsulated()
        .lossy("ISO_13818_2"),
        TransferSyntax::new(
            uids::MPEG2_MPHL,
            "MPEG2 Main Profile / High Level",
        )
        .encapsulated()
        .lossy("ISO_13818_2"),
        TransferSyntax::new(
            uids::MPEG2_MPHLF,
            "Fragmentable MPEG2 Main Profile / High Level",
        )
        .encapsulated()
        .lossy("ISO_13818_2"),
        TransferSyntax::new(
            uids::MPEG4_HP41,
            "MPEG-4 AVC/H.264 High Profile / Level 4.1",
        )
        .encapsulated()
        .lossy("ISO_14496_10"),
        TransferSyntax::new(
            uids::MPEG4_HP41_F,
            "Fragmentable MPEG-4 AVC/H.264 High Profile / Level 4.1",
        )
        .encapsulated()
        .lossy("ISO_14496_10"),
        TransferSyntax::new(
            uids::MPEG4_HP41_BD,
            "MPEG-4 AVC/H.264 BD-compatible High Profile / Level 4.1",
        )
        .encapsulated()
        .lossy("ISO_14496_10"),
        TransferSyntax::new(
            uids::MPEG4_HP41_BDF,
            "Fragmentable MPEG-4 AVC/H.264 BD-compatible High Profile / Level 4.1",
        )
        .encapsulated()
        .lossy("ISO_14496_10"),
        TransferSyntax::new(
            uids::MPEG4_HP422_D,
            "MPEG-4 AVC/H.264 High Profile / Level 4.2 For 2D Video",
        )
        .encapsulated()
        .lossy("ISO_14496_10"),
        TransferSyntax::new(
            uids::MPEG4_HP422_DF,
            "Fragmentable MPEG-4 AVC/H.264 High Profile / Level 4.2 For 2D Video",
        )
        .encapsulated()
        .lossy("ISO_14496_10"),
        TransferSyntax::new(
            uids::MPEG4_HP423_D,
            "MPEG-4 AVC/H.264 High Profile / Level 4.2 For 3D Video",
        )
        .encapsulated()
        .lossy("ISO_14496_10"),
        TransferSyntax::new(
            uids::MPEG4_HP423_DF,
            "Fragmentable MPEG-4 AVC/H.264 High Profile / Level 4.2 For 3D Video",
        )
        .encapsulated()
        .lossy("ISO_14496_10"),
        TransferSyntax::new(
            uids::MPEG4_HP42_STEREO,
            "MPEG-4 AVC/H.264 Stereo High Profile / Level 4.2",
        )
        .encapsulated()
        .lossy("ISO_14496_10"),
        TransferSyntax::new(
            uids::MPEG4_HP42_STEREOF,
            "Fragmentable MPEG-4 AVC/H.264 Stereo High Profile / Level 4.2",
        )
        .encapsulated()
        .lossy("ISO_14496_10"),
        TransferSyntax::new(
            uids::HEVCMP51,
            "HEVC/H.265 Main Profile / Level 5.1",
        )
        .encapsulated()
        .lossy("ISO_23008_2"),
        TransferSyntax::new(
            uids::HEVCM10_P51,
            "HEVC/H.265 Main 10 Profile / Level 5.1",
        )
        .encapsulated()
        .lossy("ISO_23008_2"),
        TransferSyntax::new(
            uids::HTJ2_K_LOSSLESS,
            "High-Throughput JPEG 2000 Image Compression (Lossless Only)",
        )
        .encapsulated(),
        TransferSyntax::new(
            uids::HTJ2_K_LOSSLESS_RPCL,
            "High-Throughput JPEG 2000 with RPCL Options Image Compression (Lossless Only)",
        )
        .encapsulated(),
        TransferSyntax::new(
            uids::HTJ2_K,
            "High-Throughput JPEG 2000 Image Compression",
        )
        .encapsulated()
        .lossy("ISO_15444_15"),
        TransferSyntax::new(
            uids::JPIPHTJ2_K_REFERENCED,
            "JPIP HTJ2K Referenced",
        ),
        TransferSyntax::new(
            uids::JPIPHTJ2_K_REFERENCED_DEFLATE,
            "JPIP HTJ2K Referenced Deflate",
        )
        .deflate(),
        TransferSyntax::new(
            uids::RLE_LOSSLESS,
            "RLE Lossless",
        )
        .encapsulated(),
        TransferSyntax::new(
            uids::RFC2557_MIME_ENCAPSULATION_RETIRED,
            "RFC 2557 MIME encapsulation (Retired)",
        )
        .retired(),
        TransferSyntax::new(
            uids::XML_ENCODING_RETIRED,
            "XML Encoding (Retired)",
        )
        .retired(),
        TransferSyntax::new(
            uids::PAPYRUS3_IMPLICIT_VR_LITTLE_ENDIAN_RETIRED,
            "Papyrus 3 Implicit VR Little Endian (Retired)",
        )
        .retired()
        .implicit(),
        TransferSyntax::new(
            "1.2.840.10008.1.2.2.123456",
            "Implicit VR Big Endian",
        )
        .implicit()
        .big(),
    ]
}

static ENTRIES: Lazy<HashMap<Arc<str>, Arc<TransferSyntax>>> = Lazy::new(|| {
    let mut m = HashMap::new();
    for ts in builtin_entries() {
        m.insert(ts.uid.clone(), Arc::new(ts));
    }
    m
});

static EXTENSIONS: Lazy<RwLock<HashMap<Arc<str>, Arc<TransferSyntax>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Well-known built-in syntaxes as lazily resolved constants.
pub mod syntaxes {
    use super::{TransferSyntax, ENTRIES};
    use crate::uids;
    use once_cell::sync::Lazy;
    use std::sync::Arc;

    macro_rules! ts_const {
        ($name:ident, $uid:expr) => {
            pub static $name: Lazy<Arc<TransferSyntax>> =
                Lazy::new(|| ENTRIES.get($uid).cloned().expect("built-in entry"));
        };
    }

    ts_const!(IMPLICIT_VR_LITTLE_ENDIAN, uids::IMPLICIT_VR_LITTLE_ENDIAN);
    ts_const!(EXPLICIT_VR_LITTLE_ENDIAN, uids::EXPLICIT_VR_LITTLE_ENDIAN);
    ts_const!(
        DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN,
        uids::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN
    );
    ts_const!(EXPLICIT_VR_BIG_ENDIAN, uids::EXPLICIT_VR_BIG_ENDIAN_RETIRED);
    ts_const!(JPEG_PROCESS_1, uids::JPEG_BASELINE8_BIT);
    ts_const!(JPEG_PROCESS_2_4, uids::JPEG_EXTENDED12_BIT);
    ts_const!(JPEG_LOSSLESS_PROCESS_14, uids::JPEG_LOSSLESS);
    ts_const!(JPEG_LOSSLESS_PROCESS_14_SV1, uids::JPEG_LOSSLESS_SV1);
    ts_const!(JPEG_LS_LOSSLESS, uids::JPEGLS_LOSSLESS);
    ts_const!(JPEG_LS_NEAR_LOSSLESS, uids::JPEGLS_NEAR_LOSSLESS);
    ts_const!(JPEG_2000_LOSSLESS, uids::JPEG2000_LOSSLESS);
    ts_const!(JPEG_2000_LOSSY, uids::JPEG2000);
    ts_const!(HTJ2K_LOSSLESS, uids::HTJ2_K_LOSSLESS);
    ts_const!(HTJ2K_LOSSLESS_RPCL, uids::HTJ2_K_LOSSLESS_RPCL);
    ts_const!(HTJ2K_LOSSY, uids::HTJ2_K);
    ts_const!(RLE_LOSSLESS, uids::RLE_LOSSLESS);
    ts_const!(IMPLICIT_VR_BIG_ENDIAN, "1.2.840.10008.1.2.2.123456");
    ts_const!(GE_PRIVATE_IMPLICIT_VR_BIG_ENDIAN, "1.2.840.113619.5.2");
}

/// Parses a UID string and resolves it like [`lookup`].
pub fn parse(s: &str) -> Result<Arc<TransferSyntax>, Error> {
    lookup(&Uid::parse(s))
}

/// Resolves a UID to a transfer syntax (TS-02):
/// - registered → the entry;
/// - unregistered but UID-typed TransferSyntax → constructed on the fly with
///   fo-dicom's default assumptions (explicit VR LE, encapsulated);
/// - anything else → `Err(Data)`.
pub fn lookup(uid: &Uid) -> Result<Arc<TransferSyntax>, Error> {
    if let Some(ts) = EXTENSIONS.read().expect("extensions lock").get(uid.value()) {
        return Ok(ts.clone());
    }
    if let Some(ts) = ENTRIES.get(uid.value()) {
        return Ok(ts.clone());
    }
    if uid.uid_type() == UidType::TransferSyntax {
        // fo-dicom `DicomTransferSyntax.Query` fallback (its private ctor
        // defaults) assumes explicit VR, little endian and encapsulated for an
        // unregistered TS UID, so mirror that here (review needs-verification:
        // confirmed not a defect against DicomTransferSyntax.cs).
        let ts = TransferSyntax::new(uids::EXPLICIT_VR_LITTLE_ENDIAN, "Explicit VR Little Endian")
            .encapsulated();
        let ts = Arc::new(TransferSyntax {
            uid: Arc::from(uid.value()),
            name: Arc::from(uid.name().unwrap_or("Unknown Transfer Syntax")),
            is_retired: uid.is_retired(),
            ..ts
        });
        return Ok(ts);
    }
    Err(Error::Data(DataError::Other {
        message: format!("unknown transfer syntax UID \"{}\"", uid.value()),
    }))
}

/// Non-constructing lookup (returns `None` for unknown UIDs).
pub fn query(s: &str) -> Option<Arc<TransferSyntax>> {
    let trimmed = s.trim_matches(|c| c == ' ' || c == '\0');
    EXTENSIONS
        .read()
        .expect("extensions lock")
        .get(trimmed)
        .cloned()
        .or_else(|| ENTRIES.get(trimmed).cloned())
}

/// Registration extension point (used by radicom-codecs for custom syntaxes).
/// Replaces any existing entry with the same UID.
pub fn register(
    uid: &'static str,
    name: &'static str,
    endian: Endian,
    is_explicit_vr: bool,
    is_encapsulated: bool,
) -> Arc<TransferSyntax> {
    let mut ts = TransferSyntax::new(uid, name);
    ts.endian = endian;
    ts.is_explicit_vr = is_explicit_vr;
    ts.is_encapsulated = is_encapsulated;
    let arc = Arc::new(ts);
    EXTENSIONS
        .write()
        .expect("extensions lock")
        .insert(Arc::from(uid), arc.clone());
    arc
}

/// Removes a runtime registration; built-ins cannot be unregistered.
pub fn unregister(uid: &str) -> bool {
    EXTENSIONS
        .write()
        .expect("extensions lock")
        .remove(uid)
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::syntaxes::*;
    use super::*;

    #[test]
    fn builtin_flags() {
        let implicit = IMPLICIT_VR_LITTLE_ENDIAN.clone();
        assert!(!implicit.is_explicit_vr());
        assert!(!implicit.is_encapsulated());
        assert_eq!(implicit.endian(), Endian::Little);
        assert!(!implicit.is_retired());

        let explicit = EXPLICIT_VR_LITTLE_ENDIAN.clone();
        assert!(explicit.is_explicit_vr() && !explicit.is_encapsulated());

        let be = EXPLICIT_VR_BIG_ENDIAN.clone();
        assert!(be.is_explicit_vr() && be.is_retired() && be.endian() == Endian::Big);

        let deflated = DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone();
        assert!(deflated.is_deflate() && deflated.is_explicit_vr());

        let jpeg = JPEG_PROCESS_1.clone();
        assert!(jpeg.is_encapsulated() && jpeg.is_lossy());
        assert_eq!(jpeg.lossy_compression_method(), Some("ISO_10918_1"));

        let j2k = JPEG_2000_LOSSY.clone();
        assert_eq!(j2k.lossy_compression_method(), Some("ISO_15444_1"));
        assert!(!j2k.is_retired());

        let htj2k = HTJ2K_LOSSY.clone();
        assert_eq!(htj2k.lossy_compression_method(), Some("ISO_15444_15"));

        let rle = RLE_LOSSLESS.clone();
        assert!(rle.is_encapsulated() && !rle.is_lossy());

        let jls = JPEG_LS_NEAR_LOSSLESS.clone();
        assert_eq!(jls.lossy_compression_method(), Some("ISO_14495_1"));
        let jls_lossless = JPEG_LS_LOSSLESS.clone();
        assert!(!jls_lossless.is_lossy());

        let mpeg = query("1.2.840.10008.1.2.4.100").unwrap();
        assert!(mpeg.is_encapsulated() && mpeg.is_lossy());
        assert_eq!(mpeg.lossy_compression_method(), Some("ISO_13818_2"));

        let jpip = query("1.2.840.10008.1.2.4.95").unwrap();
        assert!(jpip.is_deflate() && !jpip.is_retired() && !jpip.is_encapsulated());
    }

    #[test]
    fn virtual_syntaxes() {
        let implicit_be = IMPLICIT_VR_BIG_ENDIAN.clone();
        assert_eq!(
            implicit_be.uid(),
            "1.2.840.10008.1.2.2.123456",
            "pseudo UID"
        );
        assert!(!implicit_be.is_explicit_vr() && implicit_be.endian() == Endian::Big);

        let ge = GE_PRIVATE_IMPLICIT_VR_BIG_ENDIAN.clone();
        assert!(ge.swap_pixel_data());
        assert!(!ge.is_explicit_vr() && ge.endian() == Endian::Little);
    }

    #[test]
    fn builtin_count_in_expected_range() {
        let entries = builtin_entries();
        let count = entries.len();
        assert!((50..=60).contains(&count), "built-in count {count}");
        // all UIDs unique
        let mut uids: Vec<&str> = entries.iter().map(|t| t.uid()).collect();
        uids.sort_unstable();
        let before = uids.len();
        uids.dedup();
        assert_eq!(before, uids.len(), "UIDs are unique");
    }

    #[test]
    fn lookup_known_unknown_and_typed() {
        let known = lookup(&Uid::parse("1.2.840.10008.1.2")).unwrap();
        assert_eq!(known, *IMPLICIT_VR_LITTLE_ENDIAN);

        // Unregistered UID registered as TransferSyntax type → constructed
        let typed = Uid::register("1.2.999.7", "Custom TS", UidType::TransferSyntax, false);
        let constructed = lookup(&typed).unwrap();
        assert_eq!(constructed.uid(), "1.2.999.7");
        assert!(constructed.is_explicit_vr() && constructed.is_encapsulated());

        // Unknown-type UID → error
        let stranger = Uid::parse("1.2.888.8");
        assert!(lookup(&stranger).is_err());
    }

    #[test]
    fn query_does_not_construct() {
        assert!(query("1.2.840.10008.1.2").is_some());
        assert!(query("1.2.777.1").is_none(), "no construction via query");
    }

    #[test]
    fn register_and_unregister_extensions() {
        let custom = register("1.2.999.9", "Custom", Endian::Big, false, true);
        assert!(custom.is_encapsulated() && !custom.is_explicit_vr());
        let found = query("1.2.999.9").expect("registered");
        assert_eq!(found, custom);
        assert!(unregister("1.2.999.9"));
        assert!(query("1.2.999.9").is_none());
        assert!(
            !unregister("1.2.840.10008.1.2"),
            "built-ins cannot be unregistered"
        );
    }

    #[test]
    fn parse_string() {
        assert_eq!(
            parse("1.2.840.10008.1.2").unwrap(),
            *IMPLICIT_VR_LITTLE_ENDIAN
        );
        assert!(parse("not-a-uid").is_err());
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;
    use crate::model::uid::Uid;

    #[test]
    fn syntax_metadata_equality_and_display() {
        let ts = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        assert!(!ts.name().is_empty());
        assert_eq!(ts.uid_type(), UidType::TransferSyntax);
        let shown = format!("{ts}");
        assert!(shown.contains("1.2.840.10008.1.2.1"), "{shown}");
        assert!(lookup(&Uid::parse("1.2.840.10008.1.2.1")).is_ok());
    }
}
