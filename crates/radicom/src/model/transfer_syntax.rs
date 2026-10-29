//! Transfer syntax registry (TS-01, TS-02, TS-03; docs/01-architecture.md §2).

use crate::error::{DataError, Error};
use crate::model::endian::Endian;
use crate::model::uid::{Uid, UidType};
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

// Helper chains keep each entry on one conceptual line.
fn jpeg_lossy(uid: &'static str, name: &'static str) -> TransferSyntax {
    TransferSyntax::new(uid, name)
        .encapsulated()
        .lossy("ISO_10918_1")
        .retired()
}

fn jpeg_lossless(uid: &'static str, name: &'static str, retired: bool) -> TransferSyntax {
    let ts = TransferSyntax::new(uid, name).encapsulated();
    if retired {
        ts.retired()
    } else {
        ts
    }
}

fn video(uid: &'static str, name: &'static str, retired: bool) -> TransferSyntax {
    let ts = TransferSyntax::new(uid, name).encapsulated();
    if retired {
        ts.retired()
    } else {
        ts
    }
}

/// All built-in entries, in registry order. Video names for the 4D/Stereo
/// variants are provisional until the generated name table replaces them.
pub fn builtin_entries() -> Vec<TransferSyntax> {
    let v = vec![
        TransferSyntax::new("1.2.840.10008.1.2", "Implicit VR Little Endian").implicit(),
        TransferSyntax::new("1.2.840.10008.1.2.1", "Explicit VR Little Endian"),
        TransferSyntax::new(
            "1.2.840.10008.1.2.1.99",
            "Deflated Explicit VR Little Endian",
        )
        .deflate(),
        TransferSyntax::new("1.2.840.10008.1.2.2", "Explicit VR Big Endian")
            .big()
            .retired(),
        // JPEG still image family
        TransferSyntax::new("1.2.840.10008.1.2.4.50", "JPEG Baseline (Process 1)")
            .encapsulated()
            .lossy("ISO_10918_1"),
        TransferSyntax::new("1.2.840.10008.1.2.4.51", "JPEG Extended (Process 2 & 4)")
            .encapsulated()
            .lossy("ISO_10918_1"),
        jpeg_lossy("1.2.840.10008.1.2.4.52", "JPEG Extended (Process 3 & 5)"),
        jpeg_lossy(
            "1.2.840.10008.1.2.4.53",
            "JPEG Spectral Selection (Process 6 & 8)",
        ),
        jpeg_lossy(
            "1.2.840.10008.1.2.4.54",
            "JPEG Full Progression (Process 7 & 9)",
        ),
        jpeg_lossy("1.2.840.10008.1.2.4.55", "JPEG Lossless (Process 10 & 12)"),
        jpeg_lossy("1.2.840.10008.1.2.4.56", "JPEG Extended (Process 13 & 15)"),
        jpeg_lossless("1.2.840.10008.1.2.4.57", "JPEG Lossless (Process 14)", true),
        jpeg_lossy("1.2.840.10008.1.2.4.58", "JPEG Extended (Process 15)"),
        jpeg_lossy("1.2.840.10008.1.2.4.59", "JPEG Extended (Process 16 & 18)"),
        jpeg_lossy("1.2.840.10008.1.2.4.60", "JPEG Extended (Process 17 & 19)"),
        jpeg_lossy("1.2.840.10008.1.2.4.61", "JPEG Extended (Process 20 & 22)"),
        jpeg_lossy("1.2.840.10008.1.2.4.62", "JPEG Extended (Process 23 & 25)"),
        jpeg_lossy(
            "1.2.840.10008.1.2.4.63",
            "JPEG Spectral Selection (Process 26 & 28)",
        ),
        jpeg_lossy(
            "1.2.840.10008.1.2.4.64",
            "JPEG Full Progression (Process 29 & 31)",
        ),
        jpeg_lossy(
            "1.2.840.10008.1.2.4.65",
            "JPEG Lossless (Process 16 & 18) (Hierarchical)",
        ),
        jpeg_lossy(
            "1.2.840.10008.1.2.4.66",
            "JPEG Lossless (Process 17 & 19) (Hierarchical)",
        ),
        jpeg_lossy(
            "1.2.840.10008.1.2.4.67",
            "JPEG Lossless (Process 20 & 22) (Hierarchical)",
        ),
        jpeg_lossy(
            "1.2.840.10008.1.2.4.68",
            "JPEG Lossless (Process 23 & 25) (Hierarchical)",
        ),
        jpeg_lossy(
            "1.2.840.10008.1.2.4.69",
            "JPEG Lossless (Process 26 & 28) (Hierarchical)",
        ),
        jpeg_lossless(
            "1.2.840.10008.1.2.4.70",
            "JPEG Lossless (Process 14 Selection Value 1)",
            false,
        ),
        // JPEG-LS
        TransferSyntax::new("1.2.840.10008.1.2.4.80", "JPEG-LS Lossless").encapsulated(),
        TransferSyntax::new("1.2.840.10008.1.2.4.81", "JPEG-LS Near Lossless")
            .encapsulated()
            .lossy("ISO_14495_1"),
        // JPEG 2000
        TransferSyntax::new(
            "1.2.840.10008.1.2.4.90",
            "JPEG 2000 Image Compression (Lossless Only)",
        )
        .encapsulated(),
        TransferSyntax::new("1.2.840.10008.1.2.4.91", "JPEG 2000 Image Compression")
            .encapsulated()
            .lossy("ISO_15444_1"),
        TransferSyntax::new(
            "1.2.840.10008.1.2.4.92",
            "JPEG 2000 Part 2 Multi-component Image Compression (Lossless Only)",
        )
        .encapsulated(),
        TransferSyntax::new(
            "1.2.840.10008.1.2.4.93",
            "JPEG 2000 Part 2 Multi-component Image Compression",
        )
        .encapsulated()
        .lossy("ISO_15444_2"),
        // JPIP
        TransferSyntax::new("1.2.840.10008.1.2.4.94", "JPIP Referenced")
            .encapsulated()
            .retired(),
        TransferSyntax::new("1.2.840.10008.1.2.4.95", "JPIP Referenced (Deflate)")
            .encapsulated()
            .deflate()
            .retired(),
        // Video
        video(
            "1.2.840.10008.1.2.4.96",
            "MPEG2 Main Profile @ Main Level",
            false,
        ),
        video(
            "1.2.840.10008.1.2.4.97",
            "MPEG2 Main Profile @ High Level",
            true,
        ),
        video(
            "1.2.840.10008.1.2.4.98",
            "MPEG4 AVC/H.264 High Profile / Level 4.1",
            true,
        ),
        video(
            "1.2.840.10008.1.2.4.99",
            "MPEG4 AVC/H.264 BD-compatible High Profile / Level 4.1",
            false,
        ),
        video(
            "1.2.840.10008.1.2.4.100",
            "MPEG4 AVC/H.264 High Profile / Level 4.2 For 2D Video",
            false,
        ),
        video(
            "1.2.840.10008.1.2.4.101",
            "MPEG4 AVC/H.264 High Profile / Level 4.2 For 3D Video",
            false,
        ),
        video(
            "1.2.840.10008.1.2.4.102",
            "MPEG4 AVC/H.264 Stereo High Profile / Level 4.2",
            false,
        ),
        video(
            "1.2.840.10008.1.2.4.103",
            "HEVC/H.265 Main Profile @ Level 5.1",
            false,
        ),
        video(
            "1.2.840.10008.1.2.4.104",
            "HEVC/H.265 Main 10 Profile @ Level 5.1",
            false,
        ),
        video(
            "1.2.840.10008.1.2.4.105",
            "HEVC/H.265 Main Profile @ Level 5.1 for 4D Video",
            false,
        ),
        video(
            "1.2.840.10008.1.2.4.106",
            "HEVC/H.265 Main 10 Profile @ Level 5.1 for 4D Video",
            false,
        ),
        video(
            "1.2.840.10008.1.2.4.107",
            "HEVC/H.265 Main Profile @ Level 5.1 for 3D Video",
            false,
        ),
        // HTJ2K
        TransferSyntax::new("1.2.840.10008.1.2.4.108", "HTJ2K Lossless").encapsulated(),
        TransferSyntax::new("1.2.840.10008.1.2.4.109", "HTJ2K Lossless RPCL").encapsulated(),
        TransferSyntax::new("1.2.840.10008.1.2.4.110", "HTJ2K (Lossy)")
            .encapsulated()
            .lossy("ISO_15444_15"),
        // Other
        TransferSyntax::new("1.2.840.10008.1.2.5", "RLE Lossless").encapsulated(),
        TransferSyntax::new("1.2.840.10008.1.2.6.1", "RFC 2557 MIME encapsulation")
            .encapsulated()
            .retired(),
        TransferSyntax::new("1.2.840.10008.1.2.6.2", "XML Encoding")
            .encapsulated()
            .retired(),
        TransferSyntax::new("1.2.840.10008.1.2.7.1", "JPIP HTJ2K Referenced")
            .encapsulated()
            .retired(),
        TransferSyntax::new("1.2.840.10008.1.2.7.2", "JPIP HTJ2K Referenced (Deflate)")
            .encapsulated()
            .deflate()
            .retired(),
        TransferSyntax::new("1.2.840.10008.1.20", "Papyrus 3 Implicit VR Little Endian")
            .implicit()
            .retired(),
        // Virtual syntaxes (TS-03): not part of the standard registry
        TransferSyntax::new("1.2.840.10008.1.2.2.123456", "Implicit VR Big Endian")
            .implicit()
            .big(),
        TransferSyntax::new("1.2.840.113619.5.2", "GE Private Implicit VR Big Endian")
            .with_swap_pixel_data(),
    ];
    v
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
    use once_cell::sync::Lazy;
    use std::sync::Arc;

    macro_rules! ts_const {
        ($name:ident, $uid:literal) => {
            pub static $name: Lazy<Arc<TransferSyntax>> =
                Lazy::new(|| ENTRIES.get($uid).cloned().expect("built-in entry"));
        };
    }

    ts_const!(IMPLICIT_VR_LITTLE_ENDIAN, "1.2.840.10008.1.2");
    ts_const!(EXPLICIT_VR_LITTLE_ENDIAN, "1.2.840.10008.1.2.1");
    ts_const!(DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN, "1.2.840.10008.1.2.1.99");
    ts_const!(EXPLICIT_VR_BIG_ENDIAN, "1.2.840.10008.1.2.2");
    ts_const!(JPEG_PROCESS_1, "1.2.840.10008.1.2.4.50");
    ts_const!(JPEG_PROCESS_2_4, "1.2.840.10008.1.2.4.51");
    ts_const!(JPEG_LOSSLESS_PROCESS_14, "1.2.840.10008.1.2.4.57");
    ts_const!(JPEG_LOSSLESS_PROCESS_14_SV1, "1.2.840.10008.1.2.4.70");
    ts_const!(JPEG_LS_LOSSLESS, "1.2.840.10008.1.2.4.80");
    ts_const!(JPEG_LS_NEAR_LOSSLESS, "1.2.840.10008.1.2.4.81");
    ts_const!(JPEG_2000_LOSSLESS, "1.2.840.10008.1.2.4.90");
    ts_const!(JPEG_2000_LOSSY, "1.2.840.10008.1.2.4.91");
    ts_const!(HTJ2K_LOSSLESS, "1.2.840.10008.1.2.4.108");
    ts_const!(HTJ2K_LOSSLESS_RPCL, "1.2.840.10008.1.2.4.109");
    ts_const!(HTJ2K_LOSSY, "1.2.840.10008.1.2.4.110");
    ts_const!(RLE_LOSSLESS, "1.2.840.10008.1.2.5");
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
        let ts =
            TransferSyntax::new("1.2.840.10008.1.2.1", "Explicit VR Little Endian").encapsulated();
        let ts = Arc::new(TransferSyntax {
            uid: Arc::from(uid.value()),
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

        let mpeg = query("1.2.840.10008.1.2.4.96").unwrap();
        assert!(
            mpeg.is_encapsulated() && !mpeg.is_lossy(),
            "video is encapsulated, not flagged lossy"
        );

        let jpip = query("1.2.840.10008.1.2.4.95").unwrap();
        assert!(jpip.is_deflate() && jpip.is_retired());
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
        assert!(
            ge.is_explicit_vr() && ge.endian() == Endian::Little,
            "LE syntax with pixel swap"
        );
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
