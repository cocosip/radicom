//! DICOM UID: typing, registry and generation (UID-01..03; docs/01-architecture.md §3).

use crate::error::{DataError, Error};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

/// The 13 UID classifications (UID-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UidType {
    TransferSyntax,
    SopClass,
    MetaSopClass,
    ServiceClass,
    SopInstance,
    ApplicationContextName,
    ApplicationHostingModel,
    CodingScheme,
    FrameOfReference,
    Ldap,
    MappingResource,
    ContextGroupName,
    Unknown,
}

/// Storage classification derived for SOP Class UIDs (UID-01 heuristic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StorageCategory {
    None,
    Image,
    PresentationState,
    StructuredReport,
    Waveform,
    Document,
    Raw,
    Other,
    Private,
    Volume,
}

#[derive(Clone)]
pub struct Uid {
    value: Arc<str>,
    name: Option<&'static str>,
    uid_type: UidType,
    retired: bool,
}

impl std::fmt::Debug for Uid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uid")
            .field("value", &self.value)
            .field("name", &self.name)
            .field("uid_type", &self.uid_type)
            .field("retired", &self.retired)
            .finish()
    }
}

impl PartialEq for Uid {
    /// UIDs compare by their value string only (spec §3.1).
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for Uid {}

impl std::hash::Hash for Uid {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl PartialOrd for Uid {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Uid {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

impl std::fmt::Display for Uid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.value)
    }
}

impl AsRef<str> for Uid {
    fn as_ref(&self) -> &str {
        &self.value
    }
}

// Runtime registry: interned known UIDs plus user registrations. The
// generated table from radicom-dict is force-seeded on first use (UID-02).
static REGISTRY: Lazy<RwLock<HashMap<Arc<str>, Arc<Uid>>>> = Lazy::new(|| RwLock::new(HashMap::new()));
static SEED: Lazy<()> = Lazy::new(|| {
    let entries = radicom_dict::generated::uids::UIDS;
    let mut reg = REGISTRY.write().expect("registry lock");
    for raw in entries {
        let uid_type = match raw.type_code {
            0 => UidType::TransferSyntax,
            1 => UidType::SopClass,
            2 => UidType::MetaSopClass,
            3 => UidType::ServiceClass,
            4 => UidType::SopInstance,
            5 => UidType::ApplicationContextName,
            6 => UidType::ApplicationHostingModel,
            7 => UidType::CodingScheme,
            8 => UidType::FrameOfReference,
            9 => UidType::Ldap,
            10 => UidType::MappingResource,
            11 => UidType::ContextGroupName,
            _ => continue,
        };
        // Runtime registrations are never overwritten by the seed.
        reg.entry(Arc::from(raw.uid)).or_insert_with(|| {
            Arc::new(Uid { value: Arc::from(raw.uid), name: Some(raw.name), uid_type, retired: raw.retired })
        });
    }
});

/// Forces the generated UID table into the registry.
pub fn ensure_seeded() {
    Lazy::force(&SEED);
}

impl Uid {
    /// Builds an unregistered UID instance. Prefer [`Uid::parse`] which
    /// returns interned known instances.
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Uid { value: value.into(), name: None, uid_type: UidType::Unknown, retired: false }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn name(&self) -> Option<&'static str> {
        self.name
    }

    pub fn uid_type(&self) -> UidType {
        self.uid_type
    }

    pub fn is_retired(&self) -> bool {
        self.retired
    }

    // -- registry ---------------------------------------------------------

    /// Registers a runtime-known UID (extension point). Replaces an
    /// existing entry with the same value.
    pub fn register(value: &'static str, name: &'static str, uid_type: UidType, retired: bool) -> Arc<Uid> {
        let uid = Arc::new(Uid {
            value: Arc::from(value),
            name: Some(name),
            uid_type,
            retired,
        });
        REGISTRY.write().expect("registry lock").insert(Arc::from(value), uid.clone());
        uid
    }

    /// Parses and interns: trims trailing spaces and NULs; registry hits
    /// return the known instance; misses construct a fresh `Unknown`-typed
    /// instance (registered behavior: misses are not inserted).
    pub fn parse(s: &str) -> Arc<Uid> {
        ensure_seeded();
        let trimmed = s.trim_matches(|c| c == ' ' || c == '\0');
        if let Some(known) = REGISTRY.read().expect("registry lock").get(trimmed) {
            return known.clone();
        }
        Arc::new(Uid::new(trimmed))
    }

    /// Registry lookup without constructing (spec TS-style Query).
    pub fn query(s: &str) -> Option<Arc<Uid>> {
        ensure_seeded();
        let trimmed = s.trim_matches(|c| c == ' ' || c == '\0');
        REGISTRY.read().expect("registry lock").get(trimmed).cloned()
    }

    /// All registered UIDs sorted by value.
    pub fn enumerate() -> Vec<Arc<Uid>> {
        ensure_seeded();
        let reg = REGISTRY.read().expect("registry lock");
        let mut all: Vec<Arc<Uid>> = reg.values().cloned().collect();
        all.sort();
        all
    }

    // -- classification (UID-01) ------------------------------------------

    /// Storage category heuristic: only SOP Class UIDs classify; a private
    /// root (outside 1.2.840.10008) yields `Private`; names drive the rest.
    /// The generated table carries precise per-entry categories; this
    /// heuristic covers unknown/SEEded entries.
    pub fn storage_category(&self) -> StorageCategory {
        if self.uid_type != UidType::SopClass {
            return StorageCategory::None;
        }
        if !self.value.starts_with("1.2.840.10008.") {
            return StorageCategory::Private;
        }
        let name = self.name.unwrap_or("");
        if name.contains("Image Storage") {
            StorageCategory::Image
        } else if name.contains("Volume Storage") {
            StorageCategory::Volume
        } else if name.contains("Presentation State") {
            StorageCategory::PresentationState
        } else if name.contains("Structured Report")
            || name.contains("SR Document")
            || name.contains("SR Storage")
        {
            StorageCategory::StructuredReport
        } else if name.contains("Waveform") {
            StorageCategory::Waveform
        } else if name.contains("Document") {
            StorageCategory::Document
        } else if name.contains("Raw Data") {
            StorageCategory::Raw
        } else {
            StorageCategory::Other
        }
    }

    pub fn is_image_storage(&self) -> bool {
        self.storage_category() == StorageCategory::Image
    }

    pub fn is_volume_storage(&self) -> bool {
        self.storage_category() == StorageCategory::Volume
    }

    // -- validity (VAL UI rules, spec §9.2) -------------------------------

    /// UI validity: ≤64 chars, digits and dots only, no empty components,
    /// no component starting with `'0'` (a bare zero is invalid too,
    /// matching fo-dicom's strict rule). Trailing spaces tolerated.
    pub fn is_valid(s: &str) -> bool {
        let s = s.trim_end_matches(' ');
        if s.is_empty() || s.len() > 64 {
            return false;
        }
        if !s.chars().all(|c| c.is_ascii_digit() || c == '.') {
            return false;
        }
        // each component is digits with no leading zero, except the single "0"
        s.split('.').all(|comp| !comp.is_empty() && (comp == "0" || comp.as_bytes()[0] != b'0'))
    }

    pub fn is_valid_uid(&self) -> bool {
        Uid::is_valid(&self.value)
    }

    // -- generation (UID-03) ----------------------------------------------

    /// Generates `2.25.<decimal>` from a random UUID v4 interpreted as a
    /// 128-bit unsigned integer in RFC 4122 network byte order (PS3.5 B.2).
    pub fn generate() -> Arc<Uid> {
        Arc::new(Uid::new(Uid::generate_value()))
    }

    /// The generated UID string without interning.
    pub fn generate_value() -> String {
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        let n = u128::from_be_bytes(bytes);
        format!("2.25.{n}")
    }

    /// Appends a sequence component: `append("1.2.3", 4) == "1.2.3.4"`.
    pub fn append(base: &str, seq: u64) -> Result<String, Error> {
        let candidate = format!("{base}.{seq}");
        if Uid::is_valid(&candidate) {
            Ok(candidate)
        } else {
            Err(Error::Data(DataError::Other {
                message: format!("appended UID \"{candidate}\" is not a valid UI value"),
            }))
        }
    }
}

/// Instance-scoped consistent re-mapping generator (UID-03): repeated
/// mapping of the same source UID yields the same target UID.
#[derive(Default)]
pub struct UidGenerator {
    map: Mutex<HashMap<Arc<str>, Arc<Uid>>>,
}

impl UidGenerator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Maps `source` to a fresh generated UID, memoizing the pair.
    pub fn map(&self, source: &Uid) -> Arc<Uid> {
        let mut map = self.map.lock().expect("generator lock");
        if let Some(existing) = map.get(&source.value) {
            return existing.clone();
        }
        let generated = Uid::generate();
        map.insert(source.value.clone(), generated.clone());
        generated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_interns_registered_and_defaults_to_unknown() {
        let known = Uid::register(
            "1.2.840.10008.1.2.1",
            "Explicit VR Little Endian",
            UidType::TransferSyntax,
            false,
        );
        let parsed = Uid::parse("1.2.840.10008.1.2.1");
        assert!(Arc::ptr_eq(&known, &parsed), "parse returns the interned instance");
        assert_eq!(parsed.uid_type(), UidType::TransferSyntax);
        assert_eq!(parsed.name(), Some("Explicit VR Little Endian"));

        let unknown = Uid::parse("1.2.399.9.9");
        assert_eq!(unknown.uid_type(), UidType::Unknown);
        assert_eq!(unknown.name(), None);
        // parse misses are not inserted
        assert!(Uid::query("1.2.399.9.9").is_none());
    }

    #[test]
    fn parse_trims_padding() {
        assert_eq!(Uid::parse("1.2.3\0").value(), "1.2.3");
        assert_eq!(Uid::parse("1.2.3 ").value(), "1.2.3");
    }

    #[test]
    fn equality_by_value_only() {
        let a = Uid::register("1.2.840.10008.1.2", "Implicit VR LE", UidType::TransferSyntax, false);
        let b = Uid::parse("1.2.840.10008.1.2");
        assert_eq!(a, b, "registered vs parsed compare equal");
        assert_eq!(*a, Uid::new("1.2.840.10008.1.2"), "registered vs fresh compare by value");
    }

    #[test]
    fn enumerate_sorted() {
        Uid::register("1.2.1", "A", UidType::ServiceClass, false);
        Uid::register("1.2.0.1", "B", UidType::CodingScheme, true);
        let all = Uid::enumerate();
        assert!(all.windows(2).all(|w| w[0] <= w[1]), "sorted by value");
    }

    #[test]
    fn storage_category_heuristic() {
        let image = Uid::register(
            "1.2.840.10008.5.1.4.1.1.2",
            "CT Image Storage",
            UidType::SopClass,
            false,
        );
        assert_eq!(image.storage_category(), StorageCategory::Image);
        assert!(image.is_image_storage());

        let private_sop = Uid::register("1.3.46.670589.11.0.1", "Private SOP", UidType::SopClass, false);
        assert_eq!(private_sop.storage_category(), StorageCategory::Private);

        let ts = Uid::register("1.2.840.10008.1.2.2", "Explicit VR BE", UidType::TransferSyntax, false);
        assert_eq!(ts.storage_category(), StorageCategory::None, "non-SOP classes do not classify");

        let sr = Uid::register(
            "1.2.840.10008.5.1.4.1.1.88.11",
            "Basic Text SR Storage",
            UidType::SopClass,
            false,
        );
        assert_eq!(sr.storage_category(), StorageCategory::StructuredReport);
    }

    #[test]
    fn ui_validity_rules() {
        assert!(Uid::is_valid("1.2.840.10008.1.2"));
        assert!(Uid::is_valid("2.25.329800735698586629295641978511506172918"));
        assert!(Uid::is_valid("1.2.3 "), "trailing spaces tolerated");
        assert!(!Uid::is_valid(""), "empty");
        assert!(!Uid::is_valid(&format!("1.{}", "9".repeat(64))), ">64 chars");
        assert!(!Uid::is_valid("1.2.a"), "non-digit component");
        assert!(!Uid::is_valid("01.2"), "leading zero");
        assert!(!Uid::is_valid("1.02"), "leading zero in component");
        assert!(!Uid::is_valid("1..2"), "empty component");
        // a single "0" component is valid (PS3.5: only multi-digit leading zeros
        // are forbidden) — real roots like 1.2.826.0.1 rely on this
        assert!(Uid::is_valid("1.2.826.0.1.3680043.10.999"), "single 0 component allowed");
        assert!(!Uid::is_valid(".1"), "empty first component");
    }

    #[test]
    fn generation_is_valid_and_prefixed() {
        for _ in 0..32 {
            let uid = Uid::generate();
            let value = uid.value();
            assert!(value.starts_with("2.25."));
            assert!(uid.is_valid_uid(), "{}", uid);
            let digits = &value["2.25.".len()..];
            let n: u128 = digits.parse().expect("decimal");
            assert!(n > 0);
        }
    }

    #[test]
    fn generation_values_differ() {
        let a = Uid::generate_value();
        let b = Uid::generate_value();
        assert_ne!(a, b);
    }

    #[test]
    fn append_sequence() {
        assert_eq!(Uid::append("1.2.3", 4).unwrap(), "1.2.3.4");
        // a single "0" component is a valid root component (PS3.5), so appending
        // to it yields a valid UID rather than an error
        assert_eq!(Uid::append("0.1", 2).unwrap(), "0.1.2");
        // a genuine multi-digit leading zero is still rejected
        assert!(Uid::append("01.2", 3).is_err(), "multi-digit leading zero is invalid");
    }

    #[test]
    fn consistent_remap() {
        let gen = UidGenerator::new();
        let source = Uid::parse("1.2.840.10008.1.2");
        let a = gen.map(&source);
        let b = gen.map(&source);
        assert!(Arc::ptr_eq(&a, &b), "same source maps to the same instance");
        assert_ne!(a.as_ref().value(), source.value());
    }
}
