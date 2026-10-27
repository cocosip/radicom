//! radicom-dict — generated DICOM dictionary tables.
//!
//! Tag constants (~5347), UID constants (~1928) and dictionary lookup
//! tables, generated from the fo-dicom dictionary data (MS-PL, version
//! 2026b) by `cargo xtask gen-dict`. Inputs live in `dictionaries/` with
//! attribution; outputs in `src/generated/` are committed.

pub mod generated;

/// The bundled dictionary data version (from the fo-dicom dictionary XML).
pub const DICTIONARY_VERSION: &str = "2026b";

#[cfg(test)]
mod tests {
    #[test]
    fn public_dictionary_table_shape() {
        let exact = crate::generated::dict::DICTIONARY.len();
        let masked = crate::generated::dict::MASKED_DICTIONARY.len();
        assert_eq!(exact + masked, 5347, "public tags total");
        assert!(masked > 0, "repeating-group entries present");

        // sorted exact table → binary search finds pixel data
        let found = crate::generated::dict::DICTIONARY
            .binary_search_by_key(&(0x7FE0u16, 0x0010u16), |e| (e.group, e.element))
            .expect("PixelData present");
        let pixel = &crate::generated::dict::DICTIONARY[found];
        assert_eq!(pixel.keyword, "PixelData");
        assert_eq!(pixel.vr, "OB\\OW");
        assert_eq!(pixel.vm, "1");

        // masked overlay row matches 60xx
        let overlay = crate::generated::dict::MASKED_DICTIONARY
            .iter()
            .find(|e| e.keyword == "OverlayRows")
            .expect("OverlayRows");
        assert_eq!(overlay.group, 0x6000);
        assert_eq!(overlay.group_mask, 0xFF00);
    }

    #[test]
    fn private_dictionary_table_shape() {
        let rows = crate::generated::dict::PRIVATE_DICTIONARY.len();
        assert_eq!(rows, 4678, "private tag rows");
        let creators: std::collections::HashSet<&str> =
            crate::generated::dict::PRIVATE_DICTIONARY.iter().map(|e| e.creator).collect();
        assert_eq!(creators.len(), 235, "private creators");
    }

    #[test]
    fn uid_table_shape() {
        assert_eq!(crate::generated::uids::UIDS.len(), 1928, "registered UIDs");
        assert_eq!(crate::generated::uids::consts::VERIFICATION, "1.2.840.10008.1.1");
        let verification = crate::generated::uids::UIDS.iter().find(|u| u.uid == "1.2.840.10008.1.1").unwrap();
        assert_eq!(verification.type_code, 1, "SopClass");
        assert!(!verification.retired);
    }

    #[test]
    fn dictionary_version_is_reported() {
        assert_eq!(super::DICTIONARY_VERSION, "2026b");
    }
}
