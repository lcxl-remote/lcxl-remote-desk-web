//! Compiled personal-owner Assistant policy shared by OSS and Manager.
//!
//! This is not the organization/fleet policy counter or a grant schema version.
//! Bump it when the compiled personal Assistant authorization contract changes.
//! A new user turn may adopt it; old grants, decisions and action receipts must
//! retain their original authority. Runtime readiness and revocation are still
//! checked independently, even when this revision matches.

use crate::{
    conversation_key::{
        AssistantConversationScope, derive_assistant_conversation_key,
        is_valid_client_conversation_id,
    },
    seam::ClaimTurnParams,
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use desk_agent_protocol::{
    AgentError, AgentErrorKind, AgentScope, Capability, ExecutionMode, ai_assistant::AiAssistantAsk,
};

pub const PERSONAL_ASSISTANT_POLICY_REVISION: i64 = 1;

pub fn is_targetless_web_tool(tool: &crate::registry::RegisteredTool) -> bool {
    crate::web_research::is_web_tool(tool.name())
        || tool.name() == crate::conversation_attachment::READ_ATTACHMENT_TOOL
        || crate::subagent::tools::effect(tool.name()).is_some()
        || matches!(
            tool.effect,
            crate::registry::ToolEffect::PermissionPlanning
                | crate::registry::ToolEffect::CapabilityDiscovery
                | crate::registry::ToolEffect::SkillLoading
        )
}

pub fn targetless_web_internal_tools() -> Vec<crate::registry::RegisteredTool> {
    let mut tools = crate::permission_tools::permission_planning_tool_registry();
    tools.extend(crate::capability_disclosure::capability_discovery_tool_registry());
    tools.extend(crate::subagent::tools::registry());
    tools.extend(
        crate::conversation_history::conversation_history_tool_registry()
            .into_iter()
            .filter(|tool| tool.name() == crate::conversation_attachment::READ_ATTACHMENT_TOOL),
    );
    tools
}

pub fn validate_assistant_ask_scope(
    ask: &AiAssistantAsk,
    device_bound: bool,
) -> Result<(), &'static str> {
    if device_bound {
        return if ask.target_refs.is_empty() {
            Ok(())
        } else {
            Err("device-bound input cannot select personal conversation targets")
        };
    }
    if !ask
        .conversation_id
        .as_deref()
        .map(str::trim)
        .is_some_and(is_valid_client_conversation_id)
    {
        return Err("personal input requires a valid continuation id");
    }
    if ask.start_goal
        || ask.previous_completed_goal_id.is_some()
        || ask.temporary_conversation.is_some()
        || !ask.selected_capability_ids.is_empty()
        || (!ask.selected_attachment_ids.is_empty() && ask.target_refs.is_empty())
    {
        return Err("personal assistant input cannot carry device-bound context");
    }
    Ok(())
}

/// An owner-only conversation may admit a request without a device binding.
/// This check is shared by the stores. The host must authenticate and lock
/// the owner before accepting the request.
pub fn validate_targetless_input(
    actor_id: &str,
    run_id: &str,
    continuation_id: Option<&str>,
    scope: &AgentScope,
) -> Result<(), &'static str> {
    let continuation_id = validate_personal_input_scope(continuation_id, scope)?;
    if run_id
        != derive_assistant_conversation_key(
            actor_id,
            AssistantConversationScope::Personal,
            continuation_id,
        )
    {
        return Err("targetless input requires the personal conversation key");
    }
    Ok(())
}

/// A workbench work uses a native conversation key. Its queue binding is
/// verified separately by the host under the coordinator lock.
pub fn validate_personal_input_scope<'a>(
    continuation_id: Option<&'a str>,
    scope: &AgentScope,
) -> Result<&'a str, &'static str> {
    let continuation_id = continuation_id
        .filter(|id| is_valid_client_conversation_id(id))
        .ok_or("targetless input requires a valid continuation id")?;
    if !is_targetless_scope(scope) {
        return Err("targetless input cannot carry device authority");
    }
    Ok(continuation_id)
}

fn is_targetless_scope(scope: &AgentScope) -> bool {
    scope.mode == ExecutionMode::ReadOnly
        && scope.granted.iter().all(|capability| {
            matches!(
                capability,
                Capability::WebResearchFetch | Capability::WebResearchSearch
            )
        })
}

pub fn permits_permission_request(
    session: &PersistedAgentSession,
    request: &crate::dynamic_run::PermissionRequest,
) -> bool {
    if !session.device_id.is_empty() {
        return request.selected_read_target().is_none();
    }
    let registry = crate::ai_assistant::ai_assistant_provider_registry_ref();
    if let Some(target) = request.selected_read_target() {
        let item = &request.items[0];
        return crate::request_devices::accepts_device_reads(session)
            && session.admits_assistant_device(&target.device_id)
            && request.validate().is_ok()
            && registry
                .capability_for_tool(&item.tool_name)
                .is_some_and(|capability| {
                    crate::request_devices::selected_read_descriptor(capability)
                        && capability.wire.effect == item.expected_effect
                        && registry
                            .provider_for_capability(&capability.wire.capability_id)
                            .is_some_and(|provider| provider.wire.provider_id == item.provider_id)
                });
    }
    session.has_admitted_personal_input()
        && (request.exact_command().is_some_and(|command| {
            session
                .agent_role
                .allows_effect(crate::registry::ToolEffect::Mutating)
                && session.allows_new_mutation()
                && session.admits_assistant_device(&command.target_device_id)
                && command.actor_id == session.actor_id
                && command.input_revision == request.input_revision
                && command.policy_revision == session.policy_revision
        }) || (session.agent_role.is_main()
            && request.items.iter().all(|item| {
                crate::web_research::is_web_tool(&item.tool_name)
                    && registry
                        .capability_for_tool(&item.tool_name)
                        .is_some_and(|capability| {
                            capability.wire.effect == item.expected_effect
                                && registry
                                    .provider_for_capability(&capability.wire.capability_id)
                                    .is_some_and(|provider| {
                                        provider.wire.provider_id == item.provider_id
                                    })
                        })
            })))
}

pub fn require_current_policy(revision: i64) -> Result<(), AgentError> {
    if revision == PERSONAL_ASSISTANT_POLICY_REVISION {
        Ok(())
    } else {
        Err(AgentError {
            kind: AgentErrorKind::SessionUnavailable,
            message: "Assistant policy changed; synchronize and send a new request".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        })
    }
}

/// The owner may enable review before the first input adopts a compiled policy.
/// This does not adopt a policy or authorize a turn or device action.
pub fn require_approval_policy(session: &PersistedAgentSession) -> Result<(), AgentError> {
    if session.surface == AgentSessionSurface::AiAssistant
        && session.policy_revision == 0
        && session.input_revision == 0
        && session.conversation.is_empty()
    {
        return Ok(());
    }
    require_current_policy(session.policy_revision)
}

/// Call before changing any persisted turn/lease state. Only explicit user
/// input can start a turn under a different compiled contract; an automatic
/// continuation must keep the existing session's current policy unchanged.
pub fn validate_claim(
    surface: AgentSessionSurface,
    previous_revision: Option<i64>,
    params: &ClaimTurnParams,
) -> Result<(), AgentError> {
    if surface != AgentSessionSurface::AiAssistant {
        return Ok(());
    }
    require_current_policy(params.policy_revision)?;
    if params.device_id.is_empty()
        && (!params.trigger_origin.is_request_input()
            || !is_targetless_scope(&params.current_pdp_scope))
    {
        return Err(AgentError {
            kind: AgentErrorKind::PermissionDenied,
            message: "targetless turn cannot carry device or continuation authority".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        });
    }
    if params.trigger_origin != TriggerOrigin::User {
        require_current_policy(previous_revision.unwrap_or(0))?;
    }
    Ok(())
}

/// A host may use this only after matching the persisted owner decision and
/// original input inside the permission-resume claim transaction.
pub fn validate_permission_resume_claim(
    session: &PersistedAgentSession,
    params: &ClaimTurnParams,
    decision: &crate::dynamic_run::PermissionDecidedEvent,
) -> Result<(), AgentError> {
    if !params.device_id.is_empty() {
        return validate_claim(session.surface, Some(session.policy_revision), params);
    }
    require_current_policy(params.policy_revision)?;
    require_current_policy(session.policy_revision)?;
    use crate::dynamic_run::PermissionDecisionSource;
    let selected_request = session.permission_requests.iter().find(|request| {
        request.request_id == decision.request_id
            && request.input_revision == decision.request_input_revision
            && request.state == decision.resulting_state
            && request.selected_device_id().is_some()
            && permits_permission_request(session, request)
    });
    let authorized_decision = match &decision.decision_source {
        PermissionDecisionSource::UserDecision => true,
        PermissionDecisionSource::AutomaticReview { .. }
        | PermissionDecisionSource::SystemDecision { .. } => {
            selected_request.is_some() && session.allows_delegated_review()
        }
        PermissionDecisionSource::OwnerNoReview { authorization } => {
            selected_request.is_some_and(|request| request.exact_command().is_some())
                && session.has_direct_approval_origin()
                && authorization.root_conversation_id
                    == session.approval_delegation_conversation_id()
        }
    };
    if session.has_admitted_personal_input()
        && (session.agent_role.is_main()
            || (session.agent_role.binding().is_some() && selected_request.is_some()))
        && params.trigger_origin == TriggerOrigin::PermissionDecision
        && session.input_revision == decision.request_input_revision
        && authorized_decision
        && is_targetless_scope(&params.current_pdp_scope)
    {
        return Ok(());
    }
    Err(AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message:
            "Personal assistant resume requires the original authorized decision and request scope"
                .into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })
}

/// The host has matched the completion to its durable dispatch. Recheck the
/// original input and pending event against the session being claimed.
pub fn validate_personal_work_claim(
    session: &PersistedAgentSession,
    params: &ClaimTurnParams,
    input_revision: u64,
    event_id: &str,
) -> Result<(), AgentError> {
    require_current_policy(params.policy_revision)?;
    require_current_policy(session.policy_revision)?;
    if params.device_id.is_empty()
        && params.trigger_origin == TriggerOrigin::WorkContinuation
        && session.has_admitted_personal_input()
        && session.agent_role.is_main()
        && session.input_revision == input_revision
        && is_targetless_scope(&params.current_pdp_scope)
        && session.pending_auto_triggers.iter().any(|pending| {
            pending.event_id == event_id
                && pending.chain_id == session.chain_id
                && pending.kind == crate::session::WorkKind::AgentExec
        })
    {
        return Ok(());
    }
    Err(AgentError {
        kind: AgentErrorKind::SessionUnavailable,
        message: "The original request is no longer eligible for command continuation".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentScope, ExecutionMode};

    #[test]
    fn selectable_ask_requires_targets_when_it_selects_device_attachments() {
        let mut ask = AiAssistantAsk::default();
        assert!(validate_assistant_ask_scope(&ask, false).is_err());
        ask.conversation_id = Some("personal-thread".into());
        assert!(validate_assistant_ask_scope(&ask, false).is_ok());
        ask.target_refs.push("mac".into());
        assert!(validate_assistant_ask_scope(&ask, false).is_ok());
        assert!(validate_assistant_ask_scope(&ask, true).is_err());
        ask.selected_attachment_ids
            .push("old-device-attachment".into());
        assert!(validate_assistant_ask_scope(&ask, false).is_ok());
        ask.target_refs.clear();
        assert!(validate_assistant_ask_scope(&ask, false).is_err());
        ask.selected_attachment_ids.clear();
        ask.selected_capability_ids
            .push("device-system-info".into());
        assert!(validate_assistant_ask_scope(&ask, false).is_err());
    }

    #[test]
    fn empty_owner_conversation_can_enable_review_without_adopting_a_policy() {
        let mut session = PersistedAgentSession::new(
            "conversation",
            "owner",
            "device",
            0,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-10-09T00:00:00Z",
        );
        session.surface = AgentSessionSurface::AiAssistant;
        assert!(require_approval_policy(&session).is_ok());
        assert_eq!(session.policy_revision, 0);
        assert!(require_current_policy(session.policy_revision).is_err());
        session.input_revision = 1;
        assert!(require_approval_policy(&session).is_err());
        session.input_revision = 0;
        session.conversation.push(crate::chat::ChatMessage::text(
            "message",
            crate::chat::ChatRole::User,
            "old input",
        ));
        assert!(require_approval_policy(&session).is_err());
        session.conversation.clear();
        session.surface = AgentSessionSurface::TerminalAiAssistant;
        assert!(require_approval_policy(&session).is_err());
        session.surface = AgentSessionSurface::AiAssistant;
        for revision in [-1, PERSONAL_ASSISTANT_POLICY_REVISION + 1, i64::MAX] {
            session.policy_revision = revision;
            assert!(require_approval_policy(&session).is_err());
        }
        session.policy_revision = PERSONAL_ASSISTANT_POLICY_REVISION;
        session.input_revision = 1;
        assert!(require_approval_policy(&session).is_ok());
    }

    #[test]
    fn only_user_input_may_adopt_the_compiled_personal_policy() {
        let mut claim = ClaimTurnParams {
            conversation_id: "run".into(),
            actor_id: "owner".into(),
            device_id: "device".into(),
            policy_revision: PERSONAL_ASSISTANT_POLICY_REVISION,
            current_pdp_scope: AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            turn_id: "turn".into(),
            request_id: None,
            connection_id: None,
            now: "2026-08-30T00:00:00Z".into(),
            trigger_origin: TriggerOrigin::User,
        };
        for previous in [None, Some(0), Some(1), Some(2)] {
            assert!(validate_claim(AgentSessionSurface::AiAssistant, previous, &claim).is_ok());
            claim.trigger_origin = TriggerOrigin::PermissionDecision;
            assert_eq!(
                validate_claim(AgentSessionSurface::AiAssistant, previous, &claim).is_ok(),
                previous == Some(PERSONAL_ASSISTANT_POLICY_REVISION)
            );
            claim.trigger_origin = TriggerOrigin::ExecCompletion;
            assert_eq!(
                validate_claim(AgentSessionSurface::AiAssistant, previous, &claim).is_ok(),
                previous == Some(PERSONAL_ASSISTANT_POLICY_REVISION)
            );
            claim.trigger_origin = TriggerOrigin::User;
        }
        for invalid in [-1, 0, 2, i64::MAX] {
            claim.policy_revision = invalid;
            assert!(
                validate_claim(AgentSessionSurface::AiAssistant, Some(invalid), &claim).is_err()
            );
            assert!(
                validate_claim(
                    AgentSessionSurface::TerminalAiAssistant,
                    Some(invalid),
                    &claim
                )
                .is_ok()
            );
        }
    }
}
