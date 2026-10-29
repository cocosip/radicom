//! Extended negotiation (NET-04; docs/01-architecture.md §11).
//!
//! SOP Class Extended Negotiation carries one Service Class Application
//! Information array per SOP class: a flat byte array where the byte at index
//! `n - 1` is field `n` (PS3.7 §D.3.3.5). The AC echoes the field's agreed
//! value, or 0 to say "not supported". The curated [`AppInfo`] wrappers below
//! name the fields of the three service families that use them.
//!
//! The wire model already lives in [`crate::pdu::ExtendedNegotiation`]; this
//! module is the typed layer plus the server-side policy that decides what to
//! accept.

use crate::error::{NetworkError, Result};
use crate::pdu::{CommonExtended, ExtendedNegotiation, UserInformation};

/// Field numbers shared by all three application-info models (§11.2).
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

/// The Service Class Application Information of one SOP class (§11.2).
///
/// A flat byte array: index `n - 1` is field `n`, every field exactly one byte,
/// no length prefixes (PS3.7 §D.3.3.5). A field value of 0 means "not
/// supported"; a field the peer did not send is absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppInfo {
    pub fields: Vec<u8>,
}

impl AppInfo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a field (1-based), zero-filling any gap before it.
    pub fn with_field(mut self, field: u8, value: u8) -> Self {
        self.set_field(field, value);
        self
    }

    pub fn set_field(&mut self, field: u8, value: u8) {
        let Some(index) = field.checked_sub(1).map(usize::from) else {
            return;
        };
        if self.fields.len() <= index {
            self.fields.resize(index + 1, 0);
        }
        self.fields[index] = value;
    }

    /// The byte of `field` (1-based), or `None` when the peer sent fewer fields.
    pub fn byte_field(&self, field: u8) -> Option<u8> {
        let index = usize::from(field.checked_sub(1)?);
        self.fields.get(index).copied()
    }

    /// The one-byte value of `field`, as it travels on the wire.
    pub fn field(&self, field: u8) -> Option<&[u8]> {
        let index = usize::from(field.checked_sub(1)?);
        self.fields.get(index..index + 1)
    }

    /// A flag field: 1 = supported.
    pub fn is_supported(&self, field: u8) -> bool {
        self.byte_field(field) == Some(1)
    }

    /// Builds the answer to a request: every requested field is echoed with
    /// `accept(field, requested_value)` deciding the byte (`None` ⇒ 0, not
    /// supported).
    pub fn answer(requested: &AppInfo, mut accept: impl FnMut(u8, u8) -> Option<u8>) -> AppInfo {
        let mut out = AppInfo::new();
        for (index, &value) in requested.fields.iter().enumerate() {
            let Ok(field) = u8::try_from(index + 1) else {
                break;
            };
            out.fields.push(accept(field, value).unwrap_or(0));
        }
        out
    }
}

/// Storage Service Class application info (§11.2).
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
            .with_field(field::STORAGE_LEVEL_OF_SUPPORT, self.level_of_support)
            .with_field(field::STORAGE_DIGITAL_SIGNATURE, self.digital_signature)
            .with_field(field::STORAGE_ELEMENT_COERCION, self.element_coercion)
    }

    pub fn from_app_info(info: &AppInfo) -> Self {
        StorageAppInfo {
            level_of_support: info
                .byte_field(field::STORAGE_LEVEL_OF_SUPPORT)
                .unwrap_or(0),
            digital_signature: info
                .byte_field(field::STORAGE_DIGITAL_SIGNATURE)
                .unwrap_or(0),
            element_coercion: info
                .byte_field(field::STORAGE_ELEMENT_COERCION)
                .unwrap_or(0),
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

/// C-FIND application info (§11.2).
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
            .with_field(field::FIND_RELATIONAL_QUERIES, self.relational_queries)
            .with_field(field::FIND_DATETIME_MATCHING, self.datetime_matching)
            .with_field(field::FIND_FUZZY_MATCHING, self.fuzzy_matching)
            .with_field(field::FIND_TIMEZONE_ADJUSTMENT, self.timezone_adjustment)
            .with_field(
                field::FIND_ENHANCED_MULTIFRAME,
                self.enhanced_multiframe_conversion,
            )
    }

    pub fn from_app_info(info: &AppInfo) -> Self {
        FindAppInfo {
            relational_queries: info.byte_field(field::FIND_RELATIONAL_QUERIES).unwrap_or(0),
            datetime_matching: info.byte_field(field::FIND_DATETIME_MATCHING).unwrap_or(0),
            fuzzy_matching: info.byte_field(field::FIND_FUZZY_MATCHING).unwrap_or(0),
            timezone_adjustment: info
                .byte_field(field::FIND_TIMEZONE_ADJUSTMENT)
                .unwrap_or(0),
            enhanced_multiframe_conversion: info
                .byte_field(field::FIND_ENHANCED_MULTIFRAME)
                .unwrap_or(0),
        }
    }
}

/// C-GET / C-MOVE application info (§11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetrieveAppInfo {
    pub relational_retrieval: u8,
    pub enhanced_multiframe_conversion: u8,
}

impl RetrieveAppInfo {
    pub fn to_app_info(self) -> AppInfo {
        AppInfo::new()
            .with_field(field::RETRIEVE_RELATIONAL, self.relational_retrieval)
            .with_field(
                field::RETRIEVE_ENHANCED_MULTIFRAME,
                self.enhanced_multiframe_conversion,
            )
    }

    pub fn from_app_info(info: &AppInfo) -> Self {
        RetrieveAppInfo {
            relational_retrieval: info.byte_field(field::RETRIEVE_RELATIONAL).unwrap_or(0),
            enhanced_multiframe_conversion: info
                .byte_field(field::RETRIEVE_ENHANCED_MULTIFRAME)
                .unwrap_or(0),
        }
    }
}

/// What the SCP accepts of an extended-negotiation request (§11.2).
///
/// The default answers every requested field with 0, "not supported", which is
/// exactly what a peer must understand: it asked, and the answer says no. A
/// policy that wants to grant options fills in the fields it supports.
#[derive(Debug, Clone)]
pub enum ExtendedNegotiationPolicy {
    /// Accept nothing.
    DeclineAll,
    /// Answer each requested SOP class with `f`; `None` declines that class.
    Custom(fn(&str, &AppInfo) -> Option<AppInfo>),
}

impl ExtendedNegotiationPolicy {
    /// Accepts nothing: every field comes back as 0.
    pub fn decline_all() -> Self {
        ExtendedNegotiationPolicy::DeclineAll
    }

    /// Uses `f` to build each SOP class's answer.
    pub fn new(f: fn(&str, &AppInfo) -> Option<AppInfo>) -> Self {
        ExtendedNegotiationPolicy::Custom(f)
    }

    /// Builds the AC-side extended-negotiation items (§11.2 item 0x56).
    pub fn answer(&self, requested: &[ExtendedNegotiation]) -> Vec<ExtendedNegotiation> {
        requested
            .iter()
            .map(|ext| {
                let info = AppInfo {
                    fields: ext.requested.clone(),
                };
                let accepted = match self {
                    ExtendedNegotiationPolicy::DeclineAll => AppInfo::answer(&info, |_, _| None),
                    ExtendedNegotiationPolicy::Custom(f) => f(&ext.sop_class, &info)
                        .unwrap_or_else(|| AppInfo::answer(&info, |_, _| None)),
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

/// Applies the SCP's answer to the request's extended-negotiation items (§11.2).
///
/// Only the 0x56 items change: the AC carries `accepted`, not `requested`, and
/// the encoder already picks the right side per direction. Nothing else is
/// touched — in particular the RQ-only common extended negotiation (0x57) is
/// the caller's business, because this function cannot know which direction the
/// `UserInformation` it is given belongs to.
pub fn apply_answer(user_information: &mut UserInformation, policy: &ExtendedNegotiationPolicy) {
    user_information.extended_negotiations = policy.answer(&user_information.extended_negotiations);
}

/// Validates a common extended negotiation item (§11.2).
pub fn validate_common_extended(common: &CommonExtended) -> Result<()> {
    if common.service_class_uid.is_empty() {
        return Err(NetworkError::protocol(
            "common extended negotiation has no service class UID",
        ));
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
    fn fields_are_flat_and_gaps_are_zero_filled() {
        let info = AppInfo::new()
            .with_field(3, 1)
            .with_field(1, 2)
            .with_field(3, 9);
        assert_eq!(
            info.fields,
            vec![2, 0, 9],
            "index n-1 is field n; gaps are 0; last value wins"
        );
        assert_eq!(info.byte_field(1), Some(2));
        assert_eq!(info.byte_field(2), Some(0), "a gap reads as not-supported");
        assert_eq!(info.byte_field(4), None);
        assert_eq!(info.field(3), Some(&[9u8][..]));
    }

    #[test]
    fn storage_typed_round_trip() {
        let typed = StorageAppInfo {
            level_of_support: 1,
            digital_signature: 1,
            element_coercion: 0,
        };
        let info = typed.to_app_info();
        assert_eq!(StorageAppInfo::from_app_info(&info), typed);
        assert!(typed.peer_supports(StorageOption::DigitalSignature));
        assert!(!typed.peer_supports(StorageOption::ElementCoercion));
        assert!(typed.peer_supports(StorageOption::Level2));
    }

    #[test]
    fn find_and_retrieve_typed_round_trip() {
        let find = FindAppInfo {
            relational_queries: 1,
            datetime_matching: 1,
            ..Default::default()
        };
        assert_eq!(FindAppInfo::from_app_info(&find.to_app_info()), find);

        let retrieve = RetrieveAppInfo {
            relational_retrieval: 1,
            enhanced_multiframe_conversion: 0,
        };
        assert_eq!(
            RetrieveAppInfo::from_app_info(&retrieve.to_app_info()),
            retrieve
        );
    }

    #[test]
    fn missing_fields_read_as_not_supported() {
        let info = AppInfo::new().with_field(1, 1);
        let storage = StorageAppInfo::from_app_info(&info);
        assert_eq!(storage.level_of_support, 1);
        assert_eq!(
            storage.digital_signature, 0,
            "an absent field is not support"
        );
        assert!(!storage.peer_supports(StorageOption::DigitalSignature));
    }

    #[test]
    fn answer_echoes_every_requested_field() {
        let requested = AppInfo::new().with_field(1, 1).with_field(2, 1);
        let answer = AppInfo::answer(&requested, |field, value| {
            // grant field 1 only
            (field == 1).then_some(value)
        });
        assert_eq!(answer.field(1), Some(&[1u8][..]), "granted field is echoed");
        assert_eq!(answer.field(2), Some(&[0u8][..]), "declined field is 0");
    }

    #[test]
    fn declining_policy_answers_every_field_with_zero() {
        let requested = vec![storage_ext("1.2.840.10008.5.1.4.1.1.7", 1, 1)];
        let answered = ExtendedNegotiationPolicy::decline_all().answer(&requested);
        assert_eq!(answered.len(), 1);
        assert_eq!(answered[0].sop_class, "1.2.840.10008.5.1.4.1.1.7");
        assert!(
            answered[0].requested.is_empty(),
            "an AC item carries only the accepted side"
        );
        assert_eq!(
            answered[0].accepted,
            vec![0, 0, 0],
            "every requested field is answered with 0"
        );
    }

    #[test]
    fn declining_an_empty_request_answers_nothing() {
        // the AC mirrors the RQ: no requested item, no answered item
        let answered = ExtendedNegotiationPolicy::decline_all().answer(&[]);
        assert!(answered.is_empty());
        let empty = ExtendedNegotiation {
            sop_class: "1.2.3".into(),
            requested: Vec::new(),
            accepted: Vec::new(),
        };
        let answered = ExtendedNegotiationPolicy::decline_all().answer(&[empty]);
        assert_eq!(
            answered.len(),
            1,
            "the item itself is structurally mirrored"
        );
        assert!(answered[0].accepted.is_empty());
    }

    #[test]
    fn a_policy_can_grant_the_level_and_refuse_the_signature() {
        fn grant_level(sop_class: &str, requested: &AppInfo) -> Option<AppInfo> {
            if sop_class != "1.2.840.10008.5.1.4.1.1.7" {
                return None;
            }
            Some(AppInfo::answer(requested, |field, _| {
                (field == 1).then_some(1)
            }))
        }
        let policy = ExtendedNegotiationPolicy::new(grant_level);
        let answered = policy.answer(&[storage_ext("1.2.840.10008.5.1.4.1.1.7", 1, 1)]);
        let info = AppInfo {
            fields: answered[0].accepted.clone(),
        };
        assert!(info.is_supported(1), "level granted");
        assert_eq!(info.field(2), Some(&[0u8][..]), "signature refused");

        // a SOP class the policy does not know is answered with zeros
        let other = policy.answer(&[storage_ext("9.9.9", 1, 1)]);
        assert!(other[0].accepted.iter().all(|&value| value == 0));
    }

    #[test]
    fn apply_answer_touches_only_the_extended_items() {
        // The RQ-only common extended negotiation (0x57) is the caller's to
        // set: applying an answer must not silently drop it, or reusing an RQ's
        // user information for an AC would lose data the caller still needs.
        let mut user = UserInformation::new(16384);
        user.extended_negotiations = vec![storage_ext("1.2.3", 1, 1)];
        user.common_extended = Some(CommonExtended {
            service_class_uid: "1.2.840.10008.4.2".into(),
            related_general_sop_classes: vec!["1.2.840.10008.5.1.4.1.1.7".into()],
        });
        user.role_selections.push(RoleSelection {
            sop_class: "1.2.3".into(),
            scu_role: true,
            scp_role: false,
        });
        apply_answer(&mut user, &ExtendedNegotiationPolicy::decline_all());
        assert_eq!(
            user.extended_negotiations.len(),
            1,
            "the SOP class item survives"
        );
        assert_eq!(
            user.extended_negotiations[0].accepted,
            vec![0, 0, 0],
            "answered with 'not supported'"
        );
        assert!(
            user.common_extended.is_some(),
            "unrelated user information is left alone"
        );
        assert_eq!(user.role_selections.len(), 1);
    }

    #[test]
    fn common_extended_needs_a_service_class_and_a_related_class() {
        let empty_service = CommonExtended {
            service_class_uid: String::new(),
            related_general_sop_classes: vec!["1".into()],
        };
        assert!(validate_common_extended(&empty_service).is_err());

        let no_related = CommonExtended {
            service_class_uid: "1.2.3".into(),
            related_general_sop_classes: Vec::new(),
        };
        assert!(validate_common_extended(&no_related).is_err());

        let ok = CommonExtended {
            service_class_uid: "1.2.3".into(),
            related_general_sop_classes: vec!["1.2.4".into()],
        };
        assert!(validate_common_extended(&ok).is_ok());
    }

    #[test]
    fn role_selections_survive_the_answer() {
        // applying an answer must not disturb unrelated user items
        let mut user = UserInformation::new(16384);
        user.role_selections.push(RoleSelection {
            sop_class: "1.2.3".into(),
            scu_role: true,
            scp_role: true,
        });
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
