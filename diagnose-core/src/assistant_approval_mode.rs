//! Persisted root conversation preference. It never grants device access.

use desk_agent_protocol::ai_assistant::approval_mode::{
    AssistantApprovalMode, AssistantApprovalModeView, UpdateAssistantApprovalMode,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantApprovalPreference {
    pub mode: AssistantApprovalMode,
    pub revision: u64,
    pub no_review_confirmation_id: Option<String>,
}

impl Default for AssistantApprovalPreference {
    fn default() -> Self {
        Self {
            mode: AssistantApprovalMode::AiReview,
            revision: 1,
            no_review_confirmation_id: None,
        }
    }
}

impl AssistantApprovalPreference {
    pub fn unattended_proposal_revision(
        &self,
        session: &crate::session::PersistedAgentSession,
        request: &crate::dynamic_run::PermissionRequest,
    ) -> Option<u64> {
        (self.mode == AssistantApprovalMode::NoReview
            && session.has_direct_approval_origin()
            && request
                .exact_command()
                .is_some_and(|command| !command.is_interactive()))
        .then_some(self.revision)
    }

    pub fn decode(raw: Option<&str>) -> Option<Self> {
        let Some(raw) = raw else {
            return Some(Self::default());
        };
        let value: Self = serde_json::from_str(raw).ok()?;
        value.validate().then_some(value)
    }

    pub fn view(&self, ai_review_available: bool) -> AssistantApprovalModeView {
        AssistantApprovalModeView {
            mode: self.mode,
            revision: self.revision,
            ai_review_available,
        }
    }

    pub fn validate(&self) -> bool {
        self.revision > 0
            && self.revision <= i64::MAX as u64
            && match (&self.mode, &self.no_review_confirmation_id) {
                (AssistantApprovalMode::NoReview, Some(id)) => valid_confirmation(id),
                (AssistantApprovalMode::AiReview | AssistantApprovalMode::Manual, None) => true,
                _ => false,
            }
    }

    /// A host passes the exact pending command and the two trusted execution ceilings.
    pub fn unattended_confirmation(
        &self,
        session: &crate::session::PersistedAgentSession,
        request: &crate::dynamic_run::PermissionRequest,
        device_id: &str,
        host_ceiling: desk_agent_protocol::ExecutionMode,
        device_allows_unattended: bool,
    ) -> Option<&str> {
        use crate::dynamic_run::PermissionRequestState;
        use desk_agent_protocol::ExecutionMode;
        let command = request.exact_command()?;
        if self.mode != AssistantApprovalMode::NoReview
            || host_ceiling != ExecutionMode::Automated
            || !device_allows_unattended
            || !session.has_direct_approval_origin()
            || request.state != PermissionRequestState::Pending
            || request.input_revision != session.input_revision
            || command.target_device_id != device_id
            || command.is_interactive()
        {
            return None;
        }
        self.no_review_confirmation_id.as_deref()
    }

    pub fn unattended_decision(
        &self,
        session: &crate::session::PersistedAgentSession,
        proposal: &crate::dynamic_run::PermissionRequestedEvent,
        device_id: &str,
        host_ceiling: desk_agent_protocol::ExecutionMode,
        device_allows_unattended: bool,
        decision_event_id: String,
    ) -> Option<(
        Vec<crate::dynamic_run::PermissionDecisionItem>,
        crate::dynamic_run::PermissionDecisionSource,
    )> {
        use crate::dynamic_run::{
            PermissionDecisionItem, PermissionDecisionSource, PermissionItemDecision,
        };
        use desk_agent_protocol::capability_grant::NoReviewGrantProvenance;

        if proposal.approval_mode_revision != Some(self.revision) {
            return None;
        }
        let confirmation_id = self.unattended_confirmation(
            session,
            &proposal.request,
            device_id,
            host_ceiling,
            device_allows_unattended,
        )?;
        let [item] = proposal.request.items.as_slice() else {
            return None;
        };
        let authorization = NoReviewGrantProvenance {
            root_conversation_id: session.approval_delegation_conversation_id().into(),
            mode_revision: self.revision,
            confirmation_id: confirmation_id.into(),
            decision_event_id,
        };
        Some((
            vec![PermissionDecisionItem {
                item_id: item.item_id.clone(),
                decision: PermissionItemDecision::Approve {
                    resource_scope: item.resource_scope.clone(),
                    operation_scope: item.operation_scope.clone(),
                    export_destinations: vec![],
                    ttl_seconds: item.suggested_ttl_seconds,
                    max_uses: 1,
                },
            }],
            PermissionDecisionSource::OwnerNoReview { authorization },
        ))
    }

    pub fn matches_unattended_grant(
        &self,
        session: &crate::session::PersistedAgentSession,
        decision: &crate::dynamic_run::PermissionDecidedEvent,
        grant: &desk_agent_protocol::capability_grant::CapabilityGrant,
    ) -> bool {
        use crate::dynamic_run::{PermissionDecisionSource, PermissionRequestState};
        use desk_agent_protocol::capability_grant::CapabilityGrantIssuer;
        let CapabilityGrantIssuer::OwnerNoReview(authority) = &grant.issued_by else {
            return false;
        };
        if self.mode != AssistantApprovalMode::NoReview
            || self.revision != authority.mode_revision
            || self.no_review_confirmation_id.as_ref() != Some(&authority.confirmation_id)
            || authority.root_conversation_id != session.approval_delegation_conversation_id()
            || !session.has_direct_approval_origin()
            || session.actor_id != grant.actor_id
            || session.conversation_id != grant.run_id
            || session.input_revision != grant.input_revision
            || decision.event.run_id != grant.run_id
            || decision.event.event_id != authority.decision_event_id
            || decision.request_input_revision != grant.input_revision
            || decision.validate().is_err()
            || !matches!(&decision.decision_source, PermissionDecisionSource::OwnerNoReview { authorization } if authorization == authority)
        {
            return false;
        }
        session.permission_requests.iter().any(|request| {
            request.request_id == decision.request_id
                && request.input_revision == grant.input_revision
                && request.state == PermissionRequestState::Approved
                && request.exact_command().is_some_and(|command| {
                    !command.is_interactive()
                        && command.target_device_id == grant.target_device_id
                        && Some(&command.canonical_input_digest_sha256)
                            == grant.canonical_input_digest_sha256.as_ref()
                })
                && request.items.first().is_some_and(|item| {
                    decision
                        .items
                        .first()
                        .is_some_and(|approved| approved.item_id == item.item_id)
                        && crate::permission_grant::permission_item_grant_id(
                            &session.conversation_id,
                            request,
                            &item.item_id,
                        ) == grant.grant_id
                })
        })
    }

    pub fn update(&mut self, change: UpdateAssistantApprovalMode) -> bool {
        if self.revision != change.expected_revision || self.revision >= i64::MAX as u64 {
            return false;
        }
        let next = Self {
            mode: change.mode,
            revision: self.revision + 1,
            no_review_confirmation_id: change.no_review_confirmation_id,
        };
        if !next.validate() {
            return false;
        }
        *self = next;
        true
    }
}

pub fn preference_for_root(
    snapshot: &str,
    conversation: &str,
    actor: &str,
    device: &str,
    raw_preference: Option<&str>,
) -> Option<AssistantApprovalPreference> {
    use crate::session::{AgentSessionSurface, PersistedAgentSession};
    let session = PersistedAgentSession::decode_boxed_json(snapshot).ok()?;
    if session.surface != AgentSessionSurface::AiAssistant
        || session.conversation_id != conversation
        || session.check_subject(actor, device).is_err()
        || !session.agent_role.is_main()
        || session.agent_role.workbench_request_id().is_some()
    {
        return None;
    }
    AssistantApprovalPreference::decode(raw_preference)
}

fn valid_confirmation(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_current_owner_confirmation_is_required_to_save_no_review() {
        let mut preference = AssistantApprovalPreference::default();
        let update =
            |expected_revision, mode, confirmation: Option<&str>| UpdateAssistantApprovalMode {
                session: "root".into(),
                expected_revision,
                mode,
                no_review_confirmation_id: confirmation.map(str::to_owned),
            };
        assert_eq!(preference.mode, AssistantApprovalMode::AiReview);
        assert!(!preference.update(update(1, AssistantApprovalMode::NoReview, None)));
        assert!(!preference.update(update(1, AssistantApprovalMode::NoReview, Some(" \n"))));
        assert!(preference.update(update(
            1,
            AssistantApprovalMode::NoReview,
            Some("confirmed-1")
        )));
        assert_eq!(preference.revision, 2);
        assert!(!preference.update(update(1, AssistantApprovalMode::Manual, None)));
        assert!(preference.update(update(2, AssistantApprovalMode::Manual, None)));
        assert_eq!(preference.mode, AssistantApprovalMode::Manual);
        assert_eq!(preference.no_review_confirmation_id, None);
    }
    #[test]
    fn unattended_command_requires_original_owner_source_and_both_current_ceilings() {
        use crate::dynamic_run::{
            AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEvent, AgentRunEventKind,
            PermissionDecisionSource, PermissionRequestedEvent,
        };
        use crate::session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin};
        use desk_agent_protocol::{AgentScope, ExecutionMode};
        let time = "2026-11-05T00:00:00Z";
        let mut session = PersistedAgentSession::new(
            "run",
            "1",
            "",
            1,
            AgentScope {
                granted: Vec::new(),
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            time,
        );
        session.surface = AgentSessionSurface::AiAssistant;
        session.input_revision = 1;
        session.adopt_trigger(TriggerOrigin::User, "direct");
        let registry = crate::ai_assistant::ai_assistant_provider_registry()
            .with_command_policy(crate::command_confirmation::test_policy());
        let request = crate::permission_tools::build_permission_request(
            &crate::chat::ToolCall {
                id: "call".into(),
                name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
                arguments_json: serde_json::json!({"items":[{"item_id":"command","tool_name":"exec_command",
                    "exact_input":{"schema_version":1,"shell":"bash","command":"df -h","timeout_ms":1000},
                    "reason":"Check available disk space"}]}).to_string(),
            },
            &registry, "request".into(), 1, time.into(),
        ).unwrap();
        let mut preference = AssistantApprovalPreference::default();
        assert!(preference.update(UpdateAssistantApprovalMode {
            session: "run".into(),
            expected_revision: 1,
            mode: AssistantApprovalMode::NoReview,
            no_review_confirmation_id: Some("owner-confirmed".into()),
        }));
        let evaluate = |session: &PersistedAgentSession, device: &str, host, edge| {
            preference.unattended_confirmation(session, &request, device, host, edge)
        };
        assert_eq!(
            evaluate(&session, "device-1", ExecutionMode::Automated, true),
            Some("owner-confirmed")
        );
        assert_eq!(
            evaluate(&session, "device-2", ExecutionMode::Automated, true),
            None
        );
        assert_eq!(
            evaluate(&session, "device-1", ExecutionMode::ConfirmEachAction, true),
            None
        );
        assert_eq!(
            evaluate(&session, "device-1", ExecutionMode::Automated, false),
            None
        );
        let mut proposal = PermissionRequestedEvent {
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: "proposal".into(),
                run_id: session.conversation_id.clone(),
                event_seq: 1,
                input_revision: session.input_revision,
                kind: AgentRunEventKind::PermissionRequested,
                correlation_id: Some(request.request_id.clone()),
                source_envelope_ids: vec![],
                result_envelope_ids: vec![],
                created_at: time.into(),
            },
            request: request.clone(),
            approval_mode_revision: None,
        };
        let decide = |session: &PersistedAgentSession, proposal: &PermissionRequestedEvent| {
            preference.unattended_decision(
                session,
                proposal,
                "device-1",
                ExecutionMode::Automated,
                true,
                "decision".into(),
            )
        };
        assert!(decide(&session, &proposal).is_none());
        proposal.approval_mode_revision =
            preference.unattended_proposal_revision(&session, &request);
        let (items, source) = decide(&session, &proposal).unwrap();
        assert_eq!(items.len(), 1);
        assert!(
            matches!(source, PermissionDecisionSource::OwnerNoReview { authorization }
            if authorization.mode_revision == 2 && authorization.confirmation_id == "owner-confirmed")
        );
        session.adopt_trigger(TriggerOrigin::PermissionDecision, "resume");
        assert_eq!(
            evaluate(&session, "device-1", ExecutionMode::Automated, true),
            Some("owner-confirmed")
        );
        session.adopt_trigger(TriggerOrigin::IssueSubscription, "comment");
        session.adopt_trigger(TriggerOrigin::PermissionDecision, "reply");
        assert!(decide(&session, &proposal).is_none());
        assert_eq!(
            evaluate(&session, "device-1", ExecutionMode::Automated, true),
            None
        );
    }
}
