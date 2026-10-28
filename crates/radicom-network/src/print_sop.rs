//! Print Meta SOP class mapping and network identifier (UID-04/05;
//! docs/02-development-guide.md §2 UID-04/05).
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

/// The meta → sub SOP class table, built once at compile time (§3.6).
const TABLE: &[(&str, &[&str])] = &[
    (
        uids::BASIC_GRAYSCALE_PRINT_MANAGEMENT_META,
        &[uids::BASIC_FILM_SESSION, uids::BASIC_FILM_BOX, uids::BASIC_GRAYSCALE_IMAGE_BOX, uids::PRINTER],
    ),
    (
        uids::BASIC_COLOR_PRINT_MANAGEMENT_META,
        &[uids::BASIC_FILM_SESSION, uids::BASIC_FILM_BOX, uids::BASIC_COLOR_IMAGE_BOX, uids::PRINTER],
    ),
];

impl MetaSopClasses {
    /// Every meta-class → sub-classes entry, as fo-dicom's `Instances` table.
    pub fn instances() -> Vec<(&'static str, Vec<&'static str>)> {
        TABLE.iter().map(|(meta, subs)| (*meta, subs.to_vec())).collect()
    }

    /// The sub SOP classes aggregated by a meta SOP class, or `None` when
    /// `meta` is not a known meta class.
    pub fn sub_sop_classes(meta: &str) -> Option<Vec<&'static str>> {
        TABLE.iter().find(|(key, _)| *key == meta).map(|(_, subs)| subs.to_vec())
    }

    /// Reverse lookup: every meta SOP class whose sub-class list contains
    /// `sop_class`; when it belongs to none, returns `[sop_class]` itself
    /// (fo-dicom `GetMetaSopClass` with `DefaultIfEmpty`).
    pub fn meta_sop_classes_for(sop_class: &str) -> Vec<String> {
        let matches: Vec<String> = TABLE
            .iter()
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
/// rather than fabricating an identity. Linux reads `/sys/class/net/*/address`;
/// 64-bit Windows enumerates adapters via `GetAdaptersAddresses`; other
/// platforms currently return `None`.
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
    // Windows (64-bit): enumerate adapters via iphlpapi GetAdaptersAddresses
    #[cfg(all(target_os = "windows", target_pointer_width = "64"))]
    {
        out.extend(win_mac::adapter_macs());
    }
    out
}

/// Windows 64-bit adapter MAC enumeration via `GetAdaptersAddresses`
/// (iphlpapi). No external crate; only the leading fields of
/// `IP_ADAPTER_ADDRESSES` are read (through `PhysicalAddressLength`).
#[cfg(all(target_os = "windows", target_pointer_width = "64"))]
mod win_mac {
    /// Prefix of `IP_ADAPTER_ADDRESSES` up to `PhysicalAddressLength`.
    #[repr(C)]
    struct IpAdapterAddresses {
        length: u32,
        if_index: u32,
        next: *mut IpAdapterAddresses,
        adapter_name: *mut core::ffi::c_char,
        first_unicast: *mut core::ffi::c_void,
        first_anycast: *mut core::ffi::c_void,
        first_multicast: *mut core::ffi::c_void,
        first_dns_server: *mut core::ffi::c_void,
        dns_suffix: *mut u16,
        description: *mut u16,
        friendly_name: *mut u16,
        physical_address: [u8; 8],
        physical_address_length: u32,
    }

    #[link(name = "iphlpapi")]
    extern "system" {
        fn GetAdaptersAddresses(
            family: u32,
            flags: u32,
            reserved: *mut core::ffi::c_void,
            addresses: *mut IpAdapterAddresses,
            size: *mut u32,
        ) -> u32;
    }

    pub fn adapter_macs() -> Vec<[u8; 6]> {
        const AF_UNSPEC: u32 = 0;
        const SKIP_ANYCAST: u32 = 0x02;
        const SKIP_MULTICAST: u32 = 0x04;
        const SKIP_DNS: u32 = 0x08;
        const ERROR_BUFFER_OVERFLOW: u32 = 111;
        const NO_ERROR: u32 = 0;
        let flags = SKIP_ANYCAST | SKIP_MULTICAST | SKIP_DNS;

        let mut out = Vec::new();
        let mut size: u32 = 16 * 1024;
        for _ in 0..4 {
            // word-aligned buffer so the cast to *mut IpAdapterAddresses is sound
            let mut buf: Vec<u64> = vec![0u64; (size as usize).div_ceil(8)];
            let ret = unsafe {
                GetAdaptersAddresses(
                    AF_UNSPEC,
                    flags,
                    core::ptr::null_mut(),
                    buf.as_mut_ptr() as *mut IpAdapterAddresses,
                    &mut size,
                )
            };
            if ret == ERROR_BUFFER_OVERFLOW {
                continue; // `size` now holds the required size; retry
            }
            if ret != NO_ERROR {
                break;
            }
            let mut cur = buf.as_ptr() as *const IpAdapterAddresses;
            while !cur.is_null() {
                let adapter = unsafe { &*cur };
                if adapter.physical_address_length == 6 {
                    let mut mac = [0u8; 6];
                    mac.copy_from_slice(&adapter.physical_address[..6]);
                    out.push(mac);
                }
                cur = adapter.next;
            }
            break;
        }
        out
    }
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

    #[cfg(all(target_os = "windows", target_pointer_width = "64"))]
    #[test]
    fn windows_reads_a_real_mac_and_derives_a_valid_uid() {
        // The dev/CI host has at least one NIC; GetAdaptersAddresses must find a
        // 6-byte MAC and produce a valid DICOM UID from it.
        let uid = try_get_network_identifier("1.2.826.0.1.3680043.10.999")
            .expect("a host MAC is readable on Windows");
        assert!(uid.starts_with("1.2.826.0.1.3680043.10.999."), "uid: {uid}");
        assert!(radicom::model::uid::Uid::is_valid(&uid), "derived UID valid: {uid}");
    }
}
