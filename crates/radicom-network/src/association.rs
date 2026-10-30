//! Association model and presentation-context negotiation
//! (NET-03/04; docs/01-architecture.md §11).
//!
//! This module is pure logic: it decides which presentation contexts an SCP
//! accepts and which one a message should travel on. No IO happens here, so
//! the whole negotiation matrix is unit-testable.

use crate::error::{NetworkError, Result};
use crate::pdu::{PresentationContext, RoleSelection, UserInformation};

/// Well-known SOP class UIDs used by the negotiation rules (§11.2).
pub mod sop_class {
    pub const VERIFICATION: &str = "1.2.840.10008.1.1";
    pub const STORAGE_COMMITMENT_PUSH: &str = "1.2.840.10008.1.20.1";
    pub const PATIENT_ROOT_QR_FIND: &str = "1.2.840.10008.5.1.4.1.2.1.1";
    pub const PATIENT_ROOT_QR_MOVE: &str = "1.2.840.10008.5.1.4.1.2.1.2";
    pub const PATIENT_ROOT_QR_GET: &str = "1.2.840.10008.5.1.4.1.2.1.3";
    pub const STUDY_ROOT_QR_FIND: &str = "1.2.840.10008.5.1.4.1.2.2.1";
    pub const STUDY_ROOT_QR_MOVE: &str = "1.2.840.10008.5.1.4.1.2.2.2";
    pub const STUDY_ROOT_QR_GET: &str = "1.2.840.10008.5.1.4.1.2.2.3";
    pub const MODALITY_WORKLIST_FIND: &str = "1.2.840.10008.5.1.4.31";
}

/// Well-known transfer syntax UIDs used by the negotiation rules.
pub mod transfer_syntax {
    pub const IMPLICIT_VR_LITTLE_ENDIAN: &str = "1.2.840.10008.1.2";
    pub const EXPLICIT_VR_LITTLE_ENDIAN: &str = "1.2.840.10008.1.2.1";
    pub const DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN: &str = "1.2.840.10008.1.2.1.99";
    pub const EXPLICIT_VR_BIG_ENDIAN: &str = "1.2.840.10008.1.2.2";
}

/// Presentation-context states (§11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextState {
    Proposed,
    Accept,
    /// 1 — rejected, user-reason not specified.
    RejectUser,
    /// 2 — rejected, no reason given.
    RejectNoReason,
    /// 3 — abstract syntax not supported.
    RejectAbstractSyntaxNotSupported,
    /// 4 — transfer syntaxes not supported.
    RejectTransferSyntaxesNotSupported,
}

impl ContextState {
    /// The AC result code; `Accept` is 0.
    ///
    /// `None` for [`ContextState::Proposed`]: a proposed context has no AC
    /// result, and silently encoding one would report it as accepted.
    pub fn result_code(self) -> Option<u8> {
        Some(match self {
            ContextState::Accept => 0,
            ContextState::RejectUser => 1,
            ContextState::RejectNoReason => 2,
            ContextState::RejectAbstractSyntaxNotSupported => 3,
            ContextState::RejectTransferSyntaxesNotSupported => 4,
            ContextState::Proposed => return None,
        })
    }

    pub fn from_result_code(code: u8) -> Self {
        match code {
            0 => ContextState::Accept,
            1 => ContextState::RejectUser,
            2 => ContextState::RejectNoReason,
            3 => ContextState::RejectAbstractSyntaxNotSupported,
            _ => ContextState::RejectTransferSyntaxesNotSupported,
        }
    }

    pub fn is_accept(self) -> bool {
        self == ContextState::Accept
    }
}

/// Assigns presentation-context IDs: odd, 1, 3, 5, ... (§11.2).
#[derive(Debug, Clone)]
pub struct ContextIdAllocator {
    /// The next candidate ID; 0 marks exhaustion (IDs are odd, so 0 is free).
    next: u8,
}

impl Default for ContextIdAllocator {
    fn default() -> Self {
        ContextIdAllocator { next: 1 }
    }
}

impl ContextIdAllocator {
    /// The next free odd ID, or `None` when all 128 are taken.
    pub fn next_id(&mut self) -> Option<u8> {
        if self.next == 0 || self.next.is_multiple_of(2) {
            return None;
        }
        let id = self.next;
        // 255 is the last odd ID; anything above is out of range
        self.next = self.next.checked_add(2).unwrap_or(0);
        Some(id)
    }

    /// Reserves a specific ID (used when replaying a proposed set).
    pub fn reserve(&mut self, id: u8) {
        if id % 2 == 1 && (self.next == 0 || id >= self.next) {
            self.next = id.checked_add(2).unwrap_or(0);
        }
    }
}

/// A proposed presentation context before negotiation (§11.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedContext {
    pub id: u8,
    pub abstract_syntax: String,
    pub transfer_syntaxes: Vec<String>,
    pub roles: Vec<RoleSelection>,
}

impl ProposedContext {
    pub fn new(id: u8, abstract_syntax: impl Into<String>, transfer_syntaxes: Vec<String>) -> Self {
        ProposedContext {
            id,
            abstract_syntax: abstract_syntax.into(),
            transfer_syntaxes,
            roles: Vec::new(),
        }
    }

    /// True when this context has the same abstract syntax, transfer syntax
    /// set and roles as `other` (§11.2 dedup rule).
    pub fn is_duplicate_of(&self, other: &ProposedContext) -> bool {
        if self.abstract_syntax != other.abstract_syntax || self.roles.len() != other.roles.len() {
            return false;
        }
        let mut mine = self.transfer_syntaxes.clone();
        let mut theirs = other.transfer_syntaxes.clone();
        mine.sort();
        theirs.sort();
        if mine != theirs {
            return false;
        }
        // role pairs must match as a set
        let mut mine: Vec<&RoleSelection> = self.roles.iter().collect();
        let mut theirs: Vec<&RoleSelection> = other.roles.iter().collect();
        mine.sort_by(|a, b| a.sop_class.cmp(&b.sop_class));
        theirs.sort_by(|a, b| a.sop_class.cmp(&b.sop_class));
        mine == theirs
    }
}

/// Builds the proposed context set, deduplicating identical entries (§11.2).
#[derive(Debug, Clone, Default)]
pub struct ContextSetBuilder {
    contexts: Vec<ProposedContext>,
    allocator: ContextIdAllocator,
}

impl ContextSetBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a context, reusing an existing identical one; returns its ID.
    pub fn add(
        &mut self,
        abstract_syntax: impl Into<String>,
        transfer_syntaxes: Vec<String>,
    ) -> Result<u8> {
        let abstract_syntax = abstract_syntax.into();
        if abstract_syntax.is_empty() {
            return Err(NetworkError::protocol("abstract syntax must not be empty"));
        }
        if transfer_syntaxes.is_empty() {
            return Err(NetworkError::protocol(
                "a presentation context needs a transfer syntax",
            ));
        }
        let candidate = ProposedContext::new(0, abstract_syntax, transfer_syntaxes);
        if let Some(existing) = self.contexts.iter().find(|c| c.is_duplicate_of(&candidate)) {
            return Ok(existing.id);
        }
        let id = self
            .allocator
            .next_id()
            .ok_or(NetworkError::protocol("no presentation context IDs left"))?;
        self.contexts.push(ProposedContext { id, ..candidate });
        Ok(id)
    }

    /// Adds a context together with its role selections.
    pub fn add_with_roles(
        &mut self,
        abstract_syntax: impl Into<String>,
        transfer_syntaxes: Vec<String>,
        roles: Vec<RoleSelection>,
    ) -> Result<u8> {
        let abstract_syntax = abstract_syntax.into();
        let mut candidate =
            ProposedContext::new(0, abstract_syntax.clone(), transfer_syntaxes.clone());
        candidate.roles = roles.clone();
        if let Some(existing) = self.contexts.iter().find(|c| c.is_duplicate_of(&candidate)) {
            return Ok(existing.id);
        }
        let id = self.add(abstract_syntax, transfer_syntaxes)?;
        if let Some(context) = self.contexts.iter_mut().find(|c| c.id == id) {
            context.roles = roles;
        }
        Ok(id)
    }

    pub fn contexts(&self) -> &[ProposedContext] {
        &self.contexts
    }

    /// Converts to the wire form used in A-ASSOCIATE-RQ.
    pub fn to_wire(&self) -> Vec<PresentationContext> {
        self.contexts
            .iter()
            .map(|c| {
                PresentationContext::new(
                    c.id,
                    c.abstract_syntax.clone(),
                    c.transfer_syntaxes.clone(),
                )
            })
            .collect()
    }

    /// All role selections across the proposed contexts.
    pub fn role_selections(&self) -> Vec<RoleSelection> {
        self.contexts.iter().flat_map(|c| c.roles.clone()).collect()
    }
}

/// The SCP-side answer for one proposed context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedContext {
    pub id: u8,
    pub abstract_syntax: String,
    /// Exactly one syntax when accepted.
    pub transfer_syntax: String,
    pub state: ContextState,
    /// Role selections this side agreed to.
    pub roles: Vec<RoleSelection>,
}

impl NegotiatedContext {
    pub fn is_accepted(&self) -> bool {
        self.state.is_accept()
    }

    /// The wire form for an A-ASSOCIATE-AC item.
    pub fn to_wire(&self) -> PresentationContext {
        let mut context = PresentationContext::new(
            self.id,
            self.abstract_syntax.clone(),
            vec![self.transfer_syntax.clone()],
        );
        // a Proposed context cannot be encoded as an AC result: fall back to
        // "rejected, no reason given" so it is never reported as accepted
        context.result = self.state.result_code().unwrap_or(2);
        context
    }
}

/// How the SCP chooses among offered transfer syntaxes (§11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyntaxPriority {
    /// Offer order wins: the first offered syntax the SCP supports.
    #[default]
    Offered,
    /// SCP preference order wins.
    Preferred,
}

/// SCP-side negotiation: decides acceptance per proposed context (§11.2).
#[derive(Debug, Clone)]
pub struct NegotiationPolicy {
    /// Abstract syntaxes this SCP supports.
    pub supported_abstract: Vec<String>,
    /// Transfer syntaxes this SCP supports, in preference order.
    pub supported_syntaxes: Vec<String>,
    /// `true` to grant role selection requests.
    pub accept_roles: bool,
    pub priority: SyntaxPriority,
}

impl Default for NegotiationPolicy {
    fn default() -> Self {
        NegotiationPolicy {
            supported_abstract: Vec::new(),
            supported_syntaxes: vec![
                transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string(),
                transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.to_string(),
            ],
            accept_roles: false,
            priority: SyntaxPriority::default(),
        }
    }
}

impl NegotiationPolicy {
    /// A policy that supports exactly these abstract syntaxes.
    pub fn new(supported_abstract: Vec<String>) -> Self {
        NegotiationPolicy {
            supported_abstract,
            ..Default::default()
        }
    }

    /// Negotiates every proposed context, in order (§11.2).
    pub fn negotiate(&self, proposed: &[PresentationContext]) -> Vec<NegotiatedContext> {
        proposed.iter().map(|pc| self.negotiate_one(pc)).collect()
    }

    fn negotiate_one(&self, pc: &PresentationContext) -> NegotiatedContext {
        // abstract syntax first: an unsupported abstract is a 3-reject
        if !self
            .supported_abstract
            .iter()
            .any(|s| s == &pc.abstract_syntax)
        {
            return NegotiatedContext {
                id: pc.id,
                abstract_syntax: pc.abstract_syntax.clone(),
                // rejected contexts carry Implicit VR LE as a placeholder
                transfer_syntax: transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.to_string(),
                state: ContextState::RejectAbstractSyntaxNotSupported,
                roles: Vec::new(),
            };
        }
        let chosen = match self.priority {
            SyntaxPriority::Offered => pc
                .transfer_syntaxes
                .iter()
                .find(|s| self.supported_syntaxes.iter().any(|m| m == *s)),
            SyntaxPriority::Preferred => self
                .supported_syntaxes
                .iter()
                .find(|s| pc.transfer_syntaxes.iter().any(|o| o == *s)),
        };
        match chosen {
            Some(syntax) => NegotiatedContext {
                id: pc.id,
                abstract_syntax: pc.abstract_syntax.clone(),
                transfer_syntax: syntax.clone(),
                state: ContextState::Accept,
                roles: Vec::new(),
            },
            None => NegotiatedContext {
                id: pc.id,
                abstract_syntax: pc.abstract_syntax.clone(),
                transfer_syntax: transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.to_string(),
                state: ContextState::RejectTransferSyntaxesNotSupported,
                roles: Vec::new(),
            },
        }
    }

    /// Applies role selection: grant only when the policy allows it and the
    /// SCP supports the abstract syntax.
    pub fn negotiate_roles(&self, negotiated: &mut [NegotiatedContext], offered: &[RoleSelection]) {
        if !self.accept_roles {
            return;
        }
        for role in offered {
            for context in negotiated
                .iter_mut()
                .filter(|c| c.is_accepted() && c.abstract_syntax == role.sop_class)
            {
                context.roles.push(role.clone());
            }
        }
    }
}

/// The association's view of accepted contexts, used to route messages (§11.2).
#[derive(Debug, Clone, Default)]
pub struct AcceptedContexts {
    contexts: Vec<NegotiatedContext>,
}

impl AcceptedContexts {
    pub fn from_negotiated(contexts: Vec<NegotiatedContext>) -> Self {
        AcceptedContexts {
            contexts: contexts.into_iter().filter(|c| c.is_accepted()).collect(),
        }
    }

    pub fn all(&self) -> &[NegotiatedContext] {
        &self.contexts
    }

    pub fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }

    /// Checks incoming service, SOP class and sender role against its actual context.
    /// `requestor` identifies the association requestor, whose default role is SCU.
    pub fn validate_request(
        &self,
        id: u8,
        command: &crate::dimse::Command,
        requestor: bool,
    ) -> Result<()> {
        use crate::dimse::CommandField;
        let context = self.by_id(id).ok_or_else(|| {
            NetworkError::protocol("request on an unaccepted presentation context")
        })?;
        if command.field == CommandField::CCancelRq {
            return Ok(());
        }
        // A response has no service to dispatch; the caller diagnoses its unexpected field.
        if crate::dimse::response_field(command.field).is_none() {
            return Ok(());
        }
        let sop = if crate::nservice::uses_requested_uids(command.field) {
            command.requested_sop_class_uid.as_deref()
        } else {
            command.affected_sop_class_uid.as_deref()
        }
        .ok_or_else(|| NetworkError::protocol("request has no SOP Class UID"))?;
        let matches = sop == context.abstract_syntax
            || (crate::nservice::is_n_request(command.field)
                && crate::print_sop::MetaSopClasses::sub_sop_classes(&context.abstract_syntax)
                    .is_some_and(|classes| classes.contains(&sop)));
        if !matches {
            return Err(NetworkError::protocol(
                "request SOP Class does not match its presentation context",
            ));
        }
        // N-EVENT-REPORT is initiated by the service-class SCP (PS3.7 §10.1).
        let sender_scu_role = if command.field == CommandField::NEventReportRq {
            !requestor
        } else {
            requestor
        };
        let allowed = if context.roles.is_empty() {
            sender_scu_role
        } else {
            context.roles.iter().any(|role| {
                if sender_scu_role {
                    role.scu_role
                } else {
                    role.scp_role
                }
            })
        };
        if !allowed {
            return Err(NetworkError::protocol(
                "request sender has no negotiated role for this service",
            ));
        }
        let valid_service = if sop == sop_class::VERIFICATION {
            command.field == CommandField::CEchoRq
        } else if [
            sop_class::PATIENT_ROOT_QR_FIND,
            sop_class::STUDY_ROOT_QR_FIND,
            sop_class::MODALITY_WORKLIST_FIND,
        ]
        .contains(&sop)
        {
            command.field == CommandField::CFindRq
        } else if [
            sop_class::PATIENT_ROOT_QR_MOVE,
            sop_class::STUDY_ROOT_QR_MOVE,
        ]
        .contains(&sop)
        {
            command.field == CommandField::CMoveRq
        } else if [sop_class::PATIENT_ROOT_QR_GET, sop_class::STUDY_ROOT_QR_GET].contains(&sop) {
            command.field == CommandField::CGetRq
        } else if sop.starts_with("1.2.840.10008.5.1.4.1.1.") {
            command.field == CommandField::CStoreRq
        } else if sop.starts_with("1.2.840.10008.5.1.1.") {
            crate::nservice::is_n_request(command.field)
        } else {
            // Providers may advertise private service classes.
            command.field != CommandField::CEchoRq
        };
        if !valid_service {
            return Err(NetworkError::protocol(
                "request service is not valid for its SOP Class",
            ));
        }
        Ok(())
    }

    /// The context for an exact (abstract, syntax) pair.
    pub fn find_exact(
        &self,
        abstract_syntax: &str,
        transfer_syntax: &str,
    ) -> Option<&NegotiatedContext> {
        self.contexts
            .iter()
            .find(|c| c.abstract_syntax == abstract_syntax && c.transfer_syntax == transfer_syntax)
    }

    /// The accepted context with this presentation-context id.
    ///
    /// PDVs name their context, so this is the only correct way to choose the
    /// transfer syntax for a received dataset: the same SOP class may be
    /// accepted on several contexts with different syntaxes (review #15).
    pub fn by_id(&self, id: u8) -> Option<&NegotiatedContext> {
        self.contexts.iter().find(|c| c.id == id)
    }

    /// The first accepted context for an abstract syntax, whatever its syntax.
    pub fn find_abstract(&self, abstract_syntax: &str) -> Option<&NegotiatedContext> {
        self.contexts
            .iter()
            .find(|c| c.abstract_syntax == abstract_syntax)
    }

    /// Picks the context a message should travel on (§11.2):
    /// an exact syntax match first, then the first context of the abstract
    /// syntax, otherwise `SopClassNotSupported`.
    pub fn pick(
        &self,
        abstract_syntax: &str,
        preferred_syntax: &str,
    ) -> Result<&NegotiatedContext> {
        if let Some(context) = self.find_exact(abstract_syntax, preferred_syntax) {
            return Ok(context);
        }
        self.find_abstract(abstract_syntax)
            .ok_or_else(|| NetworkError::SopClassNotSupported {
                sop_class: abstract_syntax.to_string(),
            })
    }

    /// True when this side may act as SCU for the abstract syntax.
    pub fn can_scu(&self, abstract_syntax: &str) -> bool {
        self.contexts
            .iter()
            .any(|c| c.abstract_syntax == abstract_syntax && c.roles.iter().any(|r| r.scu_role))
    }

    /// True when the peer holds the SCP role for the abstract syntax, so this
    /// side may send it requests of that class (§11.2).
    ///
    /// This is the acceptor's view: role selection pairs are stored as they
    /// were granted to the association's requestor.
    pub fn peer_holds_scp_role(&self, abstract_syntax: &str) -> bool {
        self.contexts
            .iter()
            .any(|c| c.abstract_syntax == abstract_syntax && c.roles.iter().any(|r| r.scp_role))
    }

    /// True when role selection took part in this negotiation at all.
    ///
    /// Without it, [`Self::peer_holds_scp_role`] is false for everything simply
    /// because nobody asked: peers that ignore role selection keep working the
    /// way they always did, and only a peer that negotiated roles is held to
    /// them.
    pub fn negotiated_roles(&self) -> bool {
        self.contexts.iter().any(|c| !c.roles.is_empty())
    }
}

/// Builds the user-information item for an association request (§11.2).
pub fn build_user_information(
    max_pdu_length: u32,
    implementation_class_uid: &str,
    implementation_version_name: &str,
    roles: Vec<RoleSelection>,
) -> UserInformation {
    UserInformation {
        max_pdu_length,
        implementation_class_uid: Some(implementation_class_uid.to_string()),
        implementation_version_name: Some(implementation_version_name.to_string()),
        role_selections: roles,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pc(id: u8, abstract_syntax: &str, syntaxes: &[&str]) -> PresentationContext {
        PresentationContext::new(
            id,
            abstract_syntax,
            syntaxes.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[test]
    fn dispatch_checks_service_and_roles_but_allows_print_meta_sop_members() {
        use crate::dimse::{Command, CommandField};
        let mut policy = NegotiationPolicy::new(vec![
            sop_class::VERIFICATION.into(),
            crate::print_sop::uids::BASIC_GRAYSCALE_PRINT_MANAGEMENT_META.into(),
        ]);
        policy.accept_roles = true;
        let mut negotiated = policy.negotiate(&[
            pc(
                1,
                sop_class::VERIFICATION,
                &[transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN],
            ),
            pc(
                3,
                crate::print_sop::uids::BASIC_GRAYSCALE_PRINT_MANAGEMENT_META,
                &[transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN],
            ),
        ]);
        let mut command = Command::request(CommandField::CStoreRq, 1);
        command.affected_sop_class_uid = Some(sop_class::VERIFICATION.into());
        let accepted = AcceptedContexts::from_negotiated(negotiated.clone());
        assert!(
            accepted.validate_request(1, &command, true).is_err(),
            "C-STORE is not Verification"
        );
        command.field = CommandField::CEchoRq;
        assert!(accepted.validate_request(1, &command, true).is_ok());
        command.field = CommandField::NDeleteRq;
        command.requested_sop_class_uid = Some(sop_class::VERIFICATION.into());
        assert!(
            accepted.validate_request(1, &command, true).is_err(),
            "Verification does not provide N-DELETE"
        );
        command.field = CommandField::CEchoRq;
        assert!(
            accepted.validate_request(1, &command, false).is_err(),
            "the default acceptor role is SCP"
        );
        policy.negotiate_roles(
            &mut negotiated,
            &[RoleSelection {
                sop_class: sop_class::VERIFICATION.into(),
                scu_role: false,
                scp_role: true,
            }],
        );
        let accepted = AcceptedContexts::from_negotiated(negotiated);
        assert!(
            accepted.validate_request(1, &command, true).is_err(),
            "SCU role was explicitly denied"
        );
        assert!(accepted.validate_request(1, &command, false).is_ok());
        command.field = CommandField::NCreateRq;
        command.affected_sop_class_uid = Some(crate::print_sop::uids::BASIC_FILM_BOX.into());
        assert!(
            accepted.validate_request(3, &command, true).is_ok(),
            "the print meta-SOP includes Film Box"
        );
        command.field = CommandField::NGetRq;
        command.requested_sop_class_uid = Some("9.9.9".into());
        assert!(
            accepted.validate_request(3, &command, true).is_err(),
            "N-GET must validate Requested, not Affected, UID"
        );
    }

    #[test]
    fn event_reports_require_the_sender_scp_role_on_direct_and_meta_contexts() {
        use crate::dimse::{Command, CommandField};
        for abstract_syntax in [
            crate::print_sop::uids::PRINTER,
            crate::print_sop::uids::BASIC_GRAYSCALE_PRINT_MANAGEMENT_META,
        ] {
            let mut policy = NegotiationPolicy::new(vec![abstract_syntax.into()]);
            policy.accept_roles = true;
            let contexts = policy.negotiate(&[pc(
                1,
                abstract_syntax,
                &[transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN],
            )]);
            let mut command = Command::request(CommandField::NEventReportRq, 1);
            command.affected_sop_class_uid = Some(crate::print_sop::uids::PRINTER.into());
            let accepted = AcceptedContexts::from_negotiated(contexts.clone());
            assert!(
                accepted.validate_request(1, &command, false).is_ok(),
                "default acceptor SCP sends event reports"
            );
            assert!(
                accepted.validate_request(1, &command, true).is_err(),
                "default requestor SCU cannot send event reports"
            );
            for (scu_role, scp_role) in [(true, false), (false, true), (true, true), (false, false)]
            {
                let mut contexts = contexts.clone();
                policy.negotiate_roles(
                    &mut contexts,
                    &[RoleSelection {
                        sop_class: abstract_syntax.into(),
                        scu_role,
                        scp_role,
                    }],
                );
                let accepted = AcceptedContexts::from_negotiated(contexts);
                assert_eq!(
                    accepted.validate_request(1, &command, true).is_ok(),
                    scp_role
                );
                assert_eq!(
                    accepted.validate_request(1, &command, false).is_ok(),
                    scu_role
                );
            }
        }
    }

    #[test]
    fn role_selection_applies_to_every_context_of_the_same_sop_class() {
        let mut policy = NegotiationPolicy::new(vec!["A".into()]);
        policy.accept_roles = true;
        let mut negotiated = policy.negotiate(&[
            pc(1, "A", &[transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN]),
            pc(3, "A", &[transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN]),
        ]);
        let role = RoleSelection {
            sop_class: "A".into(),
            scu_role: false,
            scp_role: true,
        };
        policy.negotiate_roles(&mut negotiated, std::slice::from_ref(&role));
        assert!(negotiated
            .iter()
            .all(|context| context.roles == vec![role.clone()]));
    }

    #[test]
    fn context_ids_are_odd_and_ascending() {
        let mut allocator = ContextIdAllocator::default();
        let ids: Vec<u8> = (0..5).filter_map(|_| allocator.next_id()).collect();
        assert_eq!(ids, vec![1, 3, 5, 7, 9]);
        assert!(ids.iter().all(|id| id % 2 == 1));
    }

    #[test]
    fn context_ids_exhaust_at_255() {
        let mut allocator = ContextIdAllocator { next: 255 };
        assert_eq!(allocator.next_id(), Some(255));
        assert_eq!(allocator.next_id(), None, "no IDs left after 255");
        assert_eq!(allocator.next_id(), None, "stays exhausted");
    }

    #[test]
    fn duplicate_contexts_are_reused() {
        let mut builder = ContextSetBuilder::new();
        let first = builder
            .add("1.2.3", vec!["1.2.840.10008.1.2".into()])
            .unwrap();
        let again = builder
            .add("1.2.3", vec!["1.2.840.10008.1.2".into()])
            .unwrap();
        assert_eq!(first, again, "identical proposals share an ID");
        assert_eq!(builder.contexts().len(), 1);

        // a different syntax set is a different context
        let other = builder
            .add("1.2.3", vec!["1.2.840.10008.1.2.1".into()])
            .unwrap();
        assert_ne!(other, first);
        assert_eq!(builder.contexts().len(), 2);
    }

    #[test]
    fn duplicate_detection_ignores_syntax_order() {
        let mut builder = ContextSetBuilder::new();
        let a = builder.add("1.2.3", vec!["A".into(), "B".into()]).unwrap();
        let b = builder.add("1.2.3", vec!["B".into(), "A".into()]).unwrap();
        assert_eq!(a, b, "the transfer syntax set is order-insensitive");
    }

    #[test]
    fn contexts_with_roles_deduplicate_by_roles() {
        let mut builder = ContextSetBuilder::new();
        let role = RoleSelection {
            sop_class: "1.2.3".into(),
            scu_role: true,
            scp_role: false,
        };
        let a = builder
            .add_with_roles("1.2.3", vec!["S".into()], vec![role.clone()])
            .unwrap();
        let b = builder
            .add_with_roles("1.2.3", vec!["S".into()], vec![role])
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(builder.role_selections().len(), 1);

        // different roles ⇒ a separate context
        let c = builder
            .add_with_roles(
                "1.2.3",
                vec!["S".into()],
                vec![RoleSelection {
                    sop_class: "1.2.3".into(),
                    scu_role: false,
                    scp_role: true,
                }],
            )
            .unwrap();
        assert_ne!(c, a);
    }

    #[test]
    fn empty_proposals_are_rejected() {
        let mut builder = ContextSetBuilder::new();
        assert!(
            builder.add("", vec!["1.2".into()]).is_err(),
            "abstract syntax required"
        );
        assert!(
            builder.add("1.2.3", Vec::new()).is_err(),
            "transfer syntax required"
        );
    }

    #[test]
    fn scp_accepts_a_supported_pair() {
        let policy = NegotiationPolicy::new(vec!["1.2.840.10008.1.1".into()]);
        let proposed = vec![pc(1, "1.2.840.10008.1.1", &["1.2.840.10008.1.2"])];
        let negotiated = policy.negotiate(&proposed);
        assert_eq!(negotiated[0].state, ContextState::Accept);
        assert_eq!(negotiated[0].transfer_syntax, "1.2.840.10008.1.2");
        assert!(negotiated[0].is_accepted());
    }

    #[test]
    fn scp_rejects_an_unsupported_abstract_syntax() {
        let policy = NegotiationPolicy::new(vec!["1.2.840.10008.1.1".into()]);
        let proposed = vec![pc(1, "9.9.9", &["1.2.840.10008.1.2"])];
        let negotiated = policy.negotiate(&proposed);
        assert_eq!(
            negotiated[0].state,
            ContextState::RejectAbstractSyntaxNotSupported
        );
        assert_eq!(negotiated[0].state.result_code(), Some(3));
        assert_eq!(
            negotiated[0].transfer_syntax, "1.2.840.10008.1.2",
            "rejected contexts carry Implicit VR LE as a placeholder"
        );
    }

    #[test]
    fn scp_rejects_unsupported_transfer_syntaxes() {
        let policy = NegotiationPolicy::new(vec!["1.2.840.10008.1.1".into()]);
        let proposed = vec![pc(1, "1.2.840.10008.1.1", &["9.9.9.9"])];
        let negotiated = policy.negotiate(&proposed);
        assert_eq!(
            negotiated[0].state,
            ContextState::RejectTransferSyntaxesNotSupported
        );
        assert_eq!(negotiated[0].state.result_code(), Some(4));
    }

    #[test]
    fn syntax_priority_selects_by_offered_or_preferred_order() {
        // the SCP prefers explicit VR LE but the peer offers implicit first
        let mut policy = NegotiationPolicy::new(vec!["1.2.3".into()]);
        policy.supported_syntaxes = vec![
            transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN.to_string(),
            transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN.to_string(),
        ];
        let proposed = vec![pc(
            1,
            "1.2.3",
            &[
                transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN,
                transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN,
            ],
        )];

        policy.priority = SyntaxPriority::Offered;
        assert_eq!(
            policy.negotiate(&proposed)[0].transfer_syntax,
            transfer_syntax::IMPLICIT_VR_LITTLE_ENDIAN,
            "offered order wins"
        );

        policy.priority = SyntaxPriority::Preferred;
        assert_eq!(
            policy.negotiate(&proposed)[0].transfer_syntax,
            transfer_syntax::EXPLICIT_VR_LITTLE_ENDIAN,
            "SCP preference wins"
        );
    }

    #[test]
    fn every_proposed_context_gets_an_answer() {
        let policy = NegotiationPolicy::new(vec!["A".into()]);
        let proposed = vec![
            pc(1, "A", &["1.2.840.10008.1.2"]),
            pc(3, "B", &["1.2.840.10008.1.2"]),
            pc(5, "A", &["9.9.9"]),
        ];
        let negotiated = policy.negotiate(&proposed);
        assert_eq!(negotiated.len(), 3, "the AC mirrors the RQ one-for-one");
        assert!(negotiated[0].is_accepted());
        assert!(!negotiated[1].is_accepted());
        assert!(!negotiated[2].is_accepted());
        assert_eq!(
            negotiated.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![1, 3, 5]
        );
    }

    #[test]
    fn accepted_contexts_carry_exactly_one_syntax() {
        let policy = NegotiationPolicy::new(vec!["1.2.3".into()]);
        let proposed = vec![pc(
            1,
            "1.2.3",
            &["1.2.840.10008.1.2", "1.2.840.10008.1.2.1"],
        )];
        let negotiated = policy.negotiate(&proposed);
        let wire = negotiated[0].to_wire();
        assert_eq!(
            wire.transfer_syntaxes.len(),
            1,
            "an accepted PC carries one syntax"
        );
        assert_eq!(wire.result, 0);
    }

    #[test]
    fn role_selection_is_granted_only_when_allowed() {
        let proposed = vec![pc(1, "1.2.3", &["1.2.840.10008.1.2"])];
        let role = RoleSelection {
            sop_class: "1.2.3".into(),
            scu_role: true,
            scp_role: false,
        };

        let mut policy = NegotiationPolicy::new(vec!["1.2.3".into()]);
        policy.accept_roles = false;
        let mut negotiated = policy.negotiate(&proposed);
        policy.negotiate_roles(&mut negotiated, std::slice::from_ref(&role));
        assert!(negotiated[0].roles.is_empty(), "roles denied by default");

        policy.accept_roles = true;
        let mut negotiated = policy.negotiate(&proposed);
        policy.negotiate_roles(&mut negotiated, std::slice::from_ref(&role));
        assert_eq!(negotiated[0].roles.len(), 1);
    }

    #[test]
    fn roles_are_not_granted_on_rejected_contexts() {
        let mut policy = NegotiationPolicy::new(vec!["1.2.3".into()]);
        policy.accept_roles = true;
        let proposed = vec![pc(1, "9.9.9", &["1.2.840.10008.1.2"])];
        let mut negotiated = policy.negotiate(&proposed);
        policy.negotiate_roles(
            &mut negotiated,
            &[RoleSelection {
                sop_class: "9.9.9".into(),
                scu_role: true,
                scp_role: true,
            }],
        );
        assert!(
            negotiated[0].roles.is_empty(),
            "a rejected context grants nothing"
        );
    }

    #[test]
    fn context_state_codes_round_trip() {
        for state in [
            ContextState::Accept,
            ContextState::RejectUser,
            ContextState::RejectNoReason,
            ContextState::RejectAbstractSyntaxNotSupported,
            ContextState::RejectTransferSyntaxesNotSupported,
        ] {
            assert_eq!(
                ContextState::from_result_code(state.result_code().expect("an AC result")),
                state
            );
        }
        assert_eq!(ContextState::Accept.result_code(), Some(0));
        assert!(ContextState::Accept.is_accept());
        assert!(!ContextState::RejectUser.is_accept());
    }

    #[test]
    fn a_proposed_context_is_never_encoded_as_accepted() {
        // PS3.8 §9.3.3.2: only an AC result is 0/1..4; "proposed" is not one of
        // them, and writing 0 would report an unnegotiated context as accepted.
        assert_eq!(ContextState::Proposed.result_code(), None);

        let context = NegotiatedContext {
            id: 1,
            abstract_syntax: "1.2.3".into(),
            transfer_syntax: "1.2.840.10008.1.2".into(),
            state: ContextState::Proposed,
            roles: Vec::new(),
        };
        let wire = context.to_wire();
        assert_eq!(
            wire.result, 2,
            "an unnegotiated context falls back to 'rejected, no reason'"
        );
        assert!(!wire.is_accepted());
    }

    #[test]
    fn message_routing_prefers_an_exact_syntax_match() {
        let policy = NegotiationPolicy::new(vec!["A".into()]);
        let proposed = vec![
            pc(1, "A", &["1.2.840.10008.1.2"]),
            pc(3, "A", &["1.2.840.10008.1.2.1"]),
        ];
        let accepted = AcceptedContexts::from_negotiated(policy.negotiate(&proposed));
        let exact = accepted.pick("A", "1.2.840.10008.1.2.1").unwrap();
        assert_eq!(
            exact.id, 3,
            "the exact syntax match wins over the first context"
        );
    }

    #[test]
    fn message_routing_falls_back_to_the_first_context() {
        let policy = NegotiationPolicy::new(vec!["A".into()]);
        let proposed = vec![pc(1, "A", &["1.2.840.10008.1.2"])];
        let accepted = AcceptedContexts::from_negotiated(policy.negotiate(&proposed));
        let context = accepted.pick("A", "1.2.840.10008.1.2.1").unwrap();
        assert_eq!(
            context.id, 1,
            "no exact match: the first context of the abstract syntax is used"
        );
    }

    #[test]
    fn message_routing_fails_for_an_unnegotiated_sop_class() {
        let policy = NegotiationPolicy::new(vec!["A".into()]);
        let accepted = AcceptedContexts::from_negotiated(policy.negotiate(&[pc(
            1,
            "A",
            &["1.2.840.10008.1.2"],
        )]));
        match accepted.pick("B", "1.2.840.10008.1.2") {
            Err(NetworkError::SopClassNotSupported { sop_class }) => assert_eq!(sop_class, "B"),
            other => panic!("expected SopClassNotSupported, got {other:?}"),
        }
    }

    #[test]
    fn rejected_contexts_are_not_routable() {
        let policy = NegotiationPolicy::new(vec!["A".into()]);
        let accepted = AcceptedContexts::from_negotiated(policy.negotiate(&[
            pc(1, "A", &["1.2.840.10008.1.2"]),
            pc(3, "B", &["1.2.840.10008.1.2"]),
        ]));
        assert_eq!(accepted.all().len(), 1, "only accepted contexts are kept");
        assert!(accepted.find_abstract("B").is_none());
    }

    #[test]
    fn scu_role_check_reads_the_negotiated_roles() {
        let mut policy = NegotiationPolicy::new(vec!["A".into()]);
        policy.accept_roles = true;
        let proposed = vec![pc(1, "A", &["1.2.840.10008.1.2"])];
        let mut negotiated = policy.negotiate(&proposed);
        policy.negotiate_roles(
            &mut negotiated,
            &[RoleSelection {
                sop_class: "A".into(),
                scu_role: true,
                scp_role: false,
            }],
        );
        let accepted = AcceptedContexts::from_negotiated(negotiated);
        assert!(accepted.can_scu("A"));
        assert!(!accepted.can_scu("B"));
    }

    #[test]
    fn user_information_carries_identity_and_roles() {
        let roles = vec![RoleSelection {
            sop_class: "1.2.3".into(),
            scu_role: true,
            scp_role: false,
        }];
        let ui = build_user_information(16384, "1.2.826.0.1", "RADICOM_0_1", roles);
        assert_eq!(ui.max_pdu_length, 16384);
        assert_eq!(ui.implementation_class_uid.as_deref(), Some("1.2.826.0.1"));
        assert_eq!(
            ui.implementation_version_name.as_deref(),
            Some("RADICOM_0_1")
        );
        assert_eq!(ui.role_selections.len(), 1);
    }

    #[test]
    fn sop_class_constants_are_the_standard_uids() {
        assert_eq!(sop_class::VERIFICATION, "1.2.840.10008.1.1");
        assert_eq!(sop_class::STUDY_ROOT_QR_FIND, "1.2.840.10008.5.1.4.1.2.2.1");
        assert_eq!(sop_class::MODALITY_WORKLIST_FIND, "1.2.840.10008.5.1.4.31");
    }

    #[test]
    fn reserving_an_id_advances_the_allocator_past_it() {
        let mut allocator = ContextIdAllocator::default();
        assert_eq!(allocator.next_id(), Some(1));
        // replaying a proposed set reserves the IDs already handed out
        allocator.reserve(9);
        assert_eq!(allocator.next_id(), Some(11));
        allocator.reserve(12); // even IDs cannot be reserved
        assert_eq!(allocator.next_id(), Some(13));
        allocator.reserve(u8::MAX); // 255 is the last odd ID: exhaustion follows
        assert_eq!(allocator.next_id(), None);
    }

    #[test]
    fn accepted_contexts_report_emptiness() {
        let empty = AcceptedContexts::default();
        assert!(empty.is_empty());
        assert!(empty.all().is_empty());
    }
}
