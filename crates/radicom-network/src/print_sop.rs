//! Print Meta SOP class mapping and network identifier (UID-04/05;
//! docs/01-feature-list.md UID-04/05).
//!
//! `MetaSopClasses` maps a Print Management Meta SOP class to the sub SOP
//! classes it aggregates, in both directions (fo-dicom `MetaSopClasses`), and
//! `network_identifier` derives a stable DICOM UID from a host MAC address
//! (fo-dicom `NetworkManager.TryGetNetworkIdentifier`).

/// Well-known print-related SOP class UIDs (PS3.4).
pub mod uids {
    /// Basic Film Session SOP Class.
    pub const BASIC_FILM_SESSION: &str = "1.2.840.10008.5.1.1.1";
    /// Basic Film Box SOP Class.
    pub const BASIC_FILM_BOX: &str = "1.2.840.10008.5.1.1.2";
    /// Basic Grayscale Image Box SOP Class.
    pub const BASIC_GRAYSCALE_IMAGE_BOX: &str = "1.2.840.10008.5.1.1.4";
    /// Basic Color Image Box SOP Class.
    pub const BASIC_COLOR_IMAGE_BOX: &str = "1.2.840.10008.5.1.1.4.1";
    /// Printer SOP Class.
    pub const PRINTER: &str = "1.2.840.10008.5.1.1.16";
    /// Basic Grayscale Print Management Meta SOP Class.
    pub const BASIC_GRAYSCALE_PRINT_MANAGEMENT_META: &str = "1.2.840.10008.5.1.1.9";
    /// Basic Color Print Management Meta SOP Class.
    pub const BASIC_COLOR_PRINT_MANAGEMENT_META: &str = "1.2.840.10008.5.1.1.18";
}

/// Print Management Meta SOP ↔ sub SOP class mapping (UID-04).
///
/// Mirrors fo-dicom `MetaSopClasses.Instances` / `GetMetaSopClass`.
pub struct MetaSopClasses;

impl MetaSopClasses {
    /// Every meta-class → sub-classes entry, as fo-dicom's `Instances` table.
    pub fn instances() -> Vec<(&'static str, Vec<&'static str>)> {
        use uids::*;
        vec![
            (
                BASIC_GRAYSCALE_PRINT_MANAGEMENT_META,
                vec![BASIC_FILM_SESSION, BASIC_FILM_BOX, BASIC_GRAYSCALE_IMAGE_BOX, PRINTER],
            ),
            (
                BASIC_COLOR_PRINT_MANAGEMENT_META,
                vec![BASIC_FILM_SESSION, BASIC_FILM_BOX, BASIC_COLOR_IMAGE_BOX, PRINTER],
            ),
        ]
    }

    /// The sub SOP classes aggregated by a meta SOP class, or `None` when
    /// `meta` is not a known meta class.
    pub fn sub_sop_classes(meta: &str) -> Option<Vec<&'static str>> {
        Self::instances().into_iter().find(|(key, _)| *key == meta).map(|(_, subs)| subs)
    }

    /// Reverse lookup: every meta SOP class whose sub-class list contains
    /// `sop_class`; when it belongs to none, returns `[sop_class]` itself
    /// (fo-dicom `GetMetaSopClass` with `DefaultIfEmpty`).
    pub fn meta_sop_classes_for(sop_class: &str) -> Vec<String> {
        let matches: Vec<String> = Self::instances()
            .into_iter()
            .filter(|(_, subs)| subs.contains(&sop_class))
            .map(|(meta, _)| meta.to_string())
            .collect();
        if matches.is_empty() {
            vec![sop_class.to_string()]
        } else {
            matches
        }
    }
}

/// Derives a DICOM UID from a host MAC address (UID-05).
///
/// fo-dicom appends the MAC (parsed as a big-endian integer) to the
/// implementation class UID. Here `mac` is the 6 raw MAC bytes; the result is
/// `<implementation_class_uid>.<mac-as-decimal>`.
pub fn network_identifier_from_mac(implementation_class_uid: &str, mac: &[u8; 6]) -> String {
    let mac_int = mac.iter().fold(0u64, |acc, &byte| (acc << 8) | byte as u64);
    // appending a decimal number keeps the UID valid; a raw append always fits
    // because the base is a valid UID prefix and the suffix is all digits
    format!("{implementation_class_uid}.{mac_int}")
}

/// Best-effort host network identifier (UID-05, fo-dicom
/// `TryGetNetworkIdentifier`).
///
/// Reads the first non-loopback interface MAC address and derives the UID via
/// [`network_identifier_from_mac`]. Returns `None` when no MAC can be read
/// (e.g. an unsupported platform) rather than fabricating an identity. Linux
/// reads `/sys/class/net/*/address`; other platforms currently return `None`.
pub fn try_get_network_identifier(implementation_class_uid: &str) -> Option<String> {
    // avoid the loopback interface and interfaces with a zero MAC
    for mac in host_mac_addresses() {
        if mac == [0u8; 6] {
            continue;
        }
        return Some(network_identifier_from_mac(implementation_class_uid, &mac));
    }
    None
}

/// Platform MAC addresses, best-effort (no external dependency).
fn host_mac_addresses() -> Vec<[u8; 6]> {
    #[allow(unused_mut)]
    let mut out = Vec::new();
    // Linux: sysfs exposes each interface's MAC as text
    #[cfg(target_os = "linux")]
    {
        if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name == "lo" {
                    continue;
                }
                let address_path = entry.path().join("address");
                if let Ok(text) = std::fs::read_to_string(&address_path) {
                    if let Some(mac) = parse_mac(text.trim()) {
                        out.push(mac);
                    }
                }
            }
        }
    }
    out
}

/// Parses `aa:bb:cc:dd:ee:ff` into six bytes.
pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = text.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(part, 16).ok()?;
    }
    Some(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grayscale_meta_maps_to_its_sub_classes() {
        let subs = MetaSopClasses::sub_sop_classes(uids::BASIC_GRAYSCALE_PRINT_MANAGEMENT_META).unwrap();
        assert_eq!(
            subs,
            vec![
                uids::BASIC_FILM_SESSION,
                uids::BASIC_FILM_BOX,
                uids::BASIC_GRAYSCALE_IMAGE_BOX,
                uids::PRINTER,
            ]
        );
    }

    #[test]
    fn color_meta_maps_to_its_sub_classes() {
        let subs = MetaSopClasses::sub_sop_classes(uids::BASIC_COLOR_PRINT_MANAGEMENT_META).unwrap();
        assert!(subs.contains(&uids::BASIC_COLOR_IMAGE_BOX));
        assert!(!subs.contains(&uids::BASIC_GRAYSCALE_IMAGE_BOX));
    }

    #[test]
    fn a_non_meta_class_has_no_sub_classes() {
        assert!(MetaSopClasses::sub_sop_classes("1.2.840.10008.1.1").is_none());
    }

    #[test]
    fn reverse_lookup_finds_the_meta_class_for_a_sub_class() {
        // the Image Box is shared: it belongs to the grayscale meta class
        let metas = MetaSopClasses::meta_sop_classes_for(uids::BASIC_GRAYSCALE_IMAGE_BOX);
        assert_eq!(metas, vec![uids::BASIC_GRAYSCALE_PRINT_MANAGEMENT_META.to_string()]);
        // the film session is shared by both meta classes
        let metas = MetaSopClasses::meta_sop_classes_for(uids::BASIC_FILM_SESSION);
        assert_eq!(metas.len(), 2, "the film session belongs to both meta classes");
    }

    #[test]
    fn reverse_lookup_for_an_unlisted_class_returns_itself() {
        // fo-dicom's DefaultIfEmpty: a class in no meta list maps to itself
        let metas = MetaSopClasses::meta_sop_classes_for("1.2.840.10008.1.1");
        assert_eq!(metas, vec!["1.2.840.10008.1.1".to_string()]);
    }

    #[test]
    fn network_identifier_appends_the_mac_as_decimal() {
        let uid = network_identifier_from_mac("1.2.826.0.1.3680043.10.999", &[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        // 0x001122334455 = 73588229205
        assert_eq!(uid, "1.2.826.0.1.3680043.10.999.73588229205");
        assert!(radicom::model::uid::Uid::is_valid(&uid), "the derived UID is valid");
    }

    #[test]
    fn mac_parsing_accepts_colon_and_dash() {
        assert_eq!(parse_mac("aa:bb:cc:dd:ee:ff"), Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
        assert_eq!(parse_mac("00-11-22-33-44-55"), Some([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]));
        assert_eq!(parse_mac("not-a-mac"), None);
        assert_eq!(parse_mac("00:11:22"), None, "wrong number of octets");
    }
}
