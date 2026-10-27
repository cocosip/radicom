//! radicom-dict — generated DICOM dictionary tables.
//!
//! Tag constants (~5347), UID constants (~1928) and dictionary lookup
//! tables, generated from the fo-dicom dictionary XML (MS-PL, 2026b) by
//! `xtask gen-dict`. This skeleton ships the manual foundation; the
//! generated tables land with milestone M1.

/// The bundled dictionary data version (from the fo-dicom dictionary XML).
pub const DICTIONARY_VERSION: &str = "2026b";

#[cfg(test)]
mod tests {
    #[test]
    fn dictionary_version_is_reported() {
        assert_eq!(super::DICTIONARY_VERSION, "2026b");
    }
}
