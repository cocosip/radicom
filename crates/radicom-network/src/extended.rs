//! Extended negotiation (NET-04; docs/07-network.md §4.3).
//!
//! SOP Class Extended Negotiation carries an ordered `(field, bytes)` set per
//! SOP class; the AC echoes back the field's agreed value, or a field with
//! length 0 to say "not supported". The curated [`AppInfo`] wrappers below
//! name the fields of the three service families that use them.
//!
//! The wire model already lives in [`crate::pdu::ExtendedNegotiation`]; this
//! module is the typed layer plus the server-side policy that decides what to
//! accept.

use crate::error::{NetworkError, Result};
use crate::pdu::{CommonExtended, ExtendedNegotiation, UserInformation};

/// Field numbers shared by all three application-info models (§4.3).
mod field {
    /// Storage: 1 = level of support, 2 = digital signature, 3 = element coercion.
    pub const STORAGE_LEVEL_OF_SUPPORT: u8 = 1;
    pub const STORAGE_DIGITAL_SIGNATURE: u8 = 2;
    pub const STORAGE_ELEMENT_COERCION: u8 = 3;
    /// C-FIND: 1..5 relational/datetime/fuzzy/timezone/enhanced-multiframe.
    pub const FIND_RELATIONAL_QUERIES: u8 = 1;
    pub const FIND_DATETIME_MATCHING: u8 = 2;
    pub const FIND_FUZZY_MATCHING: u8 = 3;
    pub const FIND_TIMEZONE_ADJUSTMENT: u8 = 4;
    pub const FIND_ENHANCED_MULTIFRAME: u8 = 5;
    /// C-GET/C-MOVE: 1 = relational retrieval, 4 = enhanced multiframe.
    pub const RETRIEVE_RELATIONAL: u8 = 1;
    pub const RETRIEVE_ENHANCED_MULTIFRAME: u8 = 4;
}

/// The ordered application-info of one SOP class (§4.3).
///
/// Values are opaque byte strings: a field's meaning depends on the service
/// class. `bytes` empty means "do not support this field".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppInfo {
    pub fields: Vec<(u8, Vec<u8>)>,
}

impl AppInfo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces a field, keeping the insertion order of new fields.
    pub fn with_field(mut self, field: u8, value: impl Into<Vec<u8>>) -> Self {
        self.set_field(field, value);
        self
    }

    pub fn set_field(&mut self, field: u8, value: impl Into<Vec<u8>>) {
        let value = value.into();
        match self.fields.iter_mut().find(|(f, _)| *f == field) {
            Some(existing) => existing.1 = value,
            None => self.fields.push((field, value)),
        }
    }

    pub fn field(&self, field: u8) -> Option<&[u8]> {
        self.fields
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, v)| v.as_slice())
    }

    /// A single-byte field, the shape every standard field actually uses.
    pub fn byte_field(&self, field: u8) -> Option<u8> {
        self.field(field).and_then(|v| v.first().copied())
    }

    /// A flag field: 1 = supported.
    pub fn is_supported(&self, field: u8) -> bool {
        self.byte_field(field) == Some(1)
    }

    /// Builds the answer to a request: every requested field is echoed with
    /// `accept(field)` deciding the value (`None` ⇒ length 0, not supported).
    pub fn answer<'a>(
        requested: &'a AppInfo,
        mut accept: impl FnMut(u8, &'a [u8]) -> Option<Vec<u8>>,
    ) -> AppInfo {
        let mut out = AppInfo::new();
        for (field, value) in &requested.fields {
            let answer = accept(*field, value).unwrap_or_default();
            out.set_field(*field, answer);
        }
        out
    }
}

/// Storage Service Class application info (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StorageAppInfo {
    /// 1 = level 2 (coercion) supported; 0 = level 1.
    pub level_of_support: u8,
    /// 1 = digital signatures supported.
    pub digital_signature: u8,
    /// 1 = element coercion supported.
    pub element_coercion: u8,
}

impl StorageAppInfo {
    pub fn to_app_info(self) -> AppInfo {
        AppInfo::new()
            .with_field(field::STORAGE_LEVEL_OF_SUPPORT, vec![self.level_of_support])
            .with_field(field::STORAGE_DIGITAL_SIGNATURE, vec![self.digital_signature])
            .with_field(field::STORAGE_ELEMENT_COERCION, vec![self.element_coercion])
    }

    pub fn from_app_info(info: &AppInfo) -> Self {
        StorageAppInfo {
            level_of_support: info.byte_field(field::STORAGE_LEVEL_OF_SUPPORT).unwrap_or(0),
            digital_signature: info.byte_field(field::STORAGE_DIGITAL_SIGNATURE).unwrap_or(0),
            element_coercion: info.byte_field(field::STORAGE_ELEMENT_COERCION).unwrap_or(0),
        }
    }

    /// True when every requested option this side supports is also supported
    /// by the peer, i.e. the answer is not a downgrade.
    pub fn peer_supports(&self, what: StorageOption) -> bool {
        match what {
            StorageOption::DigitalSignature => self.digital_signature == 1,
            StorageOption::ElementCoercion => self.element_coercion == 1,
            StorageOption::Level2 => self.level_of_support >= 1,
        }
    }
}

/// The storage options a caller may care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageOption {
    Level2,
    DigitalSignature,
    ElementCoercion,
}

/// C-FIND application info (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FindAppInfo {
    pub relational_queries: u8,
    pub datetime_matching: u8,
    pub fuzzy_matching: u8,
    pub timezone_adjustment: u8,
    pub enhanced_multiframe_conversion: u8,
}

impl FindAppInfo {
    pub fn to_app_info(self) -> AppInfo {
        AppInfo::new()
            .with_field(field::FIND_RELATIONAL_QUERIES, vec![self.relational_queries])
            .with_field(field::FIND_DATETIME_MATCHING, vec![self.datetime_matching])
            .with_field(field::FIND_FUZZY_MATCHING, vec![self.fuzzy_matching])
            .with_field(field::FIND_TIMEZONE_ADJUSTMENT, vec![self.timezone_adjustment])
            .with_field(field::FIND_ENHANCED_MULTIFRAME, vec![self.enhanced_multiframe_conversion])
    }

    pub fn from_app_info(info: &AppInfo) -> Self {
        FindAppInfo {
            relational_queries: info.byte_field(field::FIND_RELATIONAL_QUERIES).unwrap_or(0),
            datetime_matching: info.byte_field(field::FIND_DATETIME_MATCHING).unwrap_or(0),
            fuzzy_matching: info.byte_field(field::FIND_FUZZY_MATCHING).unwrap_or(0),
            timezone_adjustment: info.byte_field(field::FIND_TIMEZONE_ADJUSTMENT).unwrap_or(0),
            enhanced_multiframe_conversion: info.byte_field(field::FIND_ENHANCED_MULTIFRAME).unwrap_or(0),
        }
    }
}

/// C-GET / C-MOVE application info (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetrieveAppInfo {
    pub relational_retrieval: u8,
    pub enhanced_multiframe_conversion: u8,
}

impl RetrieveAppInfo {
    pub fn to_app_info(self) -> AppInfo {
        AppInfo::new()
            .with_field(field::RETRIEVE_RELATIONAL, vec![self.relational_retrieval])
            .with_field(field::RETRIEVE_ENHANCED_MULTIFRAME, vec![self.enhanced_multiframe_conversion])
    }

    pub fn from_app_info(info: &AppInfo) -> Self {
        RetrieveAppInfo {
            relational_retrieval: info.byte_field(field::RETRIEVE_RELATIONAL).unwrap_or(0),
            enhanced_multiframe_conversion: info.byte_field(field::RETRIEVE_ENHANCED_MULTIFRAME).unwrap_or(0),
        }
    }
}

/// What the SCP accepts of an extended-negotiation request (§4.3).
///
/// The default answers every requested field with "not supported" (length 0),
/// which is exactly what a peer must understand: it asked, and the answer says
/// no. A policy that wants to grant options fills in the fields it supports.
/// What the SCP accepts of an extended-negotiation request (§4.3).
///
/// A declined field is answered with length 0, which is exactly what the peer
/// must understand: it asked, and the answer says no.
#[derive(Debug, Clone)]
pub enum ExtendedNegotiationPolicy {
    /// Accept nothing.
    DeclineAll,
    /// Answer each requested SOP class with `f`; `None` declines that class.
    Custom(fn(&str, &AppInfo) -> Option<AppInfo>),
}

impl ExtendedNegotiationPolicy {
    /// Accepts nothing: every field comes back with length 0.
    pub fn decline_all() -> Self {
        ExtendedNegotiationPolicy::DeclineAll
    }

    /// Uses `f` to build each SOP class's answer.
    pub fn new(f: fn(&str, &AppInfo) -> Option<AppInfo>) -> Self {
        ExtendedNegotiationPolicy::Custom(f)
    }

    /// Builds the AC-side extended-negotiation items (§3.2 item 0x56).
    pub fn answer(&self, requested: &[ExtendedNegotiation]) -> Vec<ExtendedNegotiation> {
        requested
            .iter()
            .map(|ext| {
                let info = AppInfo { fields: ext.requested.clone() };
                let accepted = match self {
                    ExtendedNegotiationPolicy::DeclineAll => AppInfo::answer(&info, |_, _| None),
                    ExtendedNegotiationPolicy::Custom(f) => {
                        f(&ext.sop_class, &info).unwrap_or_else(|| AppInfo::answer(&info, |_, _| None))
                    }
                };
                ExtendedNegotiation {
                    sop_class: ext.sop_class.clone(),
                    requested: Vec::new(),
                    accepted: accepted.fields,
                }
            })
            .collect()
    }
}

/// Applies the SCP's answer to the request's user information in place (§4.3).
///
/// The AC carries `accepted`, not `requested`: the encoder already picks the
/// right side per direction, so this only has to move the values over.
pub fn apply_answer(user_information: &mut UserInformation, policy: &ExtendedNegotiationPolicy) {
    let answered = policy.answer(&user_information.extended_negotiations);
    user_information.extended_negotiations = answered;
    // common extended negotiation is RQ-only (PS3.7), so the AC never echoes it
    user_information.common_extended = None;
}

/// Validates a common extended negotiation item (§4.3).
pub fn validate_common_extended(common: &CommonExtended) -> Result<()> {
    if common.service_class_uid.is_empty() {
        return Err(NetworkError::protocol("common extended negotiation has no service class UID"));
    }
    if common.related_general_sop_classes.is_empty() {
        return Err(NetworkError::protocol(
            "common extended negotiation must name at least one related general SOP class",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdu::{RoleSelection, UserIdentity};

    fn storage_ext(sop_class: &str, level: u8, signature: u8) -> ExtendedNegotiation {
        ExtendedNegotiation {
            sop_class: sop_class.into(),
            requested: StorageAppInfo {
                level_of_support: level,
                digital_signature: signature,
                element_coercion: 0,
            }
            .to_app_info()
            .fields,
            accepted: Vec::new(),
        }
    }

    #[test]
    fn app_info_keeps_field_order_and_replaces_in_place() {
        let info = AppInfo::new().with_field(3, vec![1]).with_field(1, vec![2]).with_field(3, vec![9]);
        assert_eq!(info.fields, vec![(3, vec![9]), (1, vec![2])], "order of first insertion, last value wins");
        assert_eq!(info.byte_field(1), Some(2));
        assert_eq!(info.byte_field(7), None);
    }

    #[test]
    fn storage_typed_round_trip() {
        let typed = StorageAppInfo { level_of_support: 1, digital_signature: 1, element_coercion: 0 };
        let info = typed.to_app_info();
        assert_eq!(StorageAppInfo::from_app_info(&info), typed);
        assert!(typed.peer_supports(StorageOption::DigitalSignature));
        assert!(!typed.peer_supports(StorageOption::ElementCoercion));
        assert!(typed.peer_supports(StorageOption::Level2));
    }

    #[test]
    fn find_and_retrieve_typed_round_trip() {
        let find = FindAppInfo { relational_queries: 1, datetime_matching: 1, ..Default::default() };
        assert_eq!(FindAppInfo::from_app_info(&find.to_app_info()), find);

        let retrieve = RetrieveAppInfo { relational_retrieval: 1, enhanced_multiframe_conversion: 0 };
        assert_eq!(RetrieveAppInfo::from_app_info(&retrieve.to_app_info()), retrieve);
    }

    #[test]
    fn missing_fields_read_as_not_supported() {
        let info = AppInfo::new().with_field(1, vec![1]);
        let storage = StorageAppInfo::from_app_info(&info);
        assert_eq!(storage.level_of_support, 1);
        assert_eq!(storage.digital_signature, 0, "an absent field is not support");
        assert!(!storage.peer_supports(StorageOption::DigitalSignature));
    }

    #[test]
    fn answer_echoes_every_requested_field() {
        let requested = AppInfo::new().with_field(1, vec![1]).with_field(2, vec![1]);
        let answer = AppInfo::answer(&requested, |field, value| {
            // grant field 1 only
            if field == 1 { Some(value.to_vec()) } else { None }
        });
        assert_eq!(answer.field(1), Some(&[1u8][..]), "granted field is echoed");
        assert_eq!(answer.field(2), Some(&[][..]), "declined field has zero length");
    }

    #[test]
    fn declining_policy_answers_with_zero_length_fields() {
        let requested = vec![storage_ext("1.2.840.10008.5.1.4.1.1.7", 1, 1)];
        let answered = ExtendedNegotiationPolicy::decline_all().answer(&requested);
        assert_eq!(answered.len(), 1);
        assert_eq!(answered[0].sop_class, "1.2.840.10008.5.1.4.1.1.7");
        assert!(answered[0].requested.is_empty(), "an AC item carries only the accepted side");
        assert_eq!(answered[0].accepted.len(), 3, "every requested field is answered");
        assert!(
            answered[0].accepted.iter().all(|(_, value)| value.is_empty()),
            "length 0 means not supported"
        );
    }

    #[test]
    fn declining_an_empty_request_answers_nothing() {
        // the AC mirrors the RQ: no requested item, no answered item
        let answered = ExtendedNegotiationPolicy::decline_all().answer(&[]);
        assert!(answered.is_empty());
        let empty = ExtendedNegotiation { sop_class: "1.2.3".into(), requested: Vec::new(), accepted: Vec::new() };
        let answered = ExtendedNegotiationPolicy::decline_all().answer(&[empty]);
        assert_eq!(answered.len(), 1, "the item itself is structurally mirrored");
        assert!(answered[0].accepted.is_empty());
    }

    #[test]
    fn a_policy_can_grant_the_level_and_refuse_the_signature() {
        fn grant_level(sop_class: &str, requested: &AppInfo) -> Option<AppInfo> {
            if sop_class != "1.2.840.10008.5.1.4.1.1.7" {
                return None;
            }
            Some(AppInfo::answer(requested, |field, _| (field == 1).then(|| vec![1])))
        }
        let policy = ExtendedNegotiationPolicy::new(grant_level);
        let answered = policy.answer(&[storage_ext("1.2.840.10008.5.1.4.1.1.7", 1, 1)]);
        let info = AppInfo { fields: answered[0].accepted.clone() };
        assert!(info.is_supported(1), "level granted");
        assert_eq!(info.field(2), Some(&[][..]), "signature refused");

        // a SOP class the policy does not know is answered with zero lengths
        let other = policy.answer(&[storage_ext("9.9.9", 1, 1)]);
        assert!(other[0].accepted.iter().all(|(_, v)| v.is_empty()));
    }

    #[test]
    fn common_extended_is_rq_only() {
        let mut user = UserInformation::new(16384);
        user.extended_negotiations = vec![storage_ext("1.2.3", 1, 1)];
        user.common_extended = Some(CommonExtended {
            service_class_uid: "1.2.840.10008.4.2".into(),
            related_general_sop_classes: vec!["1.2.840.10008.5.1.4.1.1.7".into()],
        });
        apply_answer(&mut user, &ExtendedNegotiationPolicy::decline_all());
        assert!(user.common_extended.is_none(), "the AC does not echo it");
        assert_eq!(user.extended_negotiations.len(), 1, "the SOP class item survives");
    }

    #[test]
    fn common_extended_needs_a_service_class_and_a_related_class() {
        let empty_service = CommonExtended { service_class_uid: String::new(), related_general_sop_classes: vec!["1".into()] };
        assert!(validate_common_extended(&empty_service).is_err());

        let no_related = CommonExtended { service_class_uid: "1.2.3".into(), related_general_sop_classes: Vec::new() };
        assert!(validate_common_extended(&no_related).is_err());

        let ok = CommonExtended { service_class_uid: "1.2.3".into(), related_general_sop_classes: vec!["1.2.4".into()] };
        assert!(validate_common_extended(&ok).is_ok());
    }

    #[test]
    fn role_selections_survive_the_answer() {
        // applying an answer must not disturb unrelated user items
        let mut user = UserInformation::new(16384);
        user.role_selections.push(RoleSelection { sop_class: "1.2.3".into(), scu_role: true, scp_role: true });
        user.user_identity = Some(UserIdentity {
            identity_type: 1,
            primary: b"user".to_vec(),
            secondary: Vec::new(),
            positive_response_requested: true,
        });
        apply_answer(&mut user, &ExtendedNegotiationPolicy::decline_all());
        assert_eq!(user.role_selections.len(), 1);
        assert!(user.user_identity.is_some());
    }
}
