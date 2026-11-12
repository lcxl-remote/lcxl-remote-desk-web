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
    tool.name() == crate::ai_assistant::PREVIEW_COMPUTER_ACTION_TOOL
        || crate::web_research::is_web_tool(tool.name())
        || tool.name() == crate::conversation_attachment::READ_ATTACHMENT_TOOL
        || crate::subagent::tools::effect(tool.name()).is_some()
        || matches!(
            tool.effect,
            crate::registry::ToolEffect::PermissionPlanning
                | crate::registry::ToolEffect::CapabilityDiscovery
                | crate::registry::ToolEffect::SkillLoading
                | crate::registry::ToolEffect::SchedulePlanning
                | crate::registry::ToolEffect::ScheduleQuery
                | crate::registry::ToolEffect::GoalControl
                | crate::registry::ToolEffect::GoalOpenPlanning
        )
}

pub fn targetless_web_internal_tools() -> Vec<crate::registry::RegisteredTool> {
    let mut tools = crate::capability_disclosure::capability_discovery_tool_registry();
    tools.extend(crate::subagent::tools::registry());
    tools.extend(crate::schedule::proposal::registry());
    tools.extend(crate::goal_tools::registry());
    tools.extend(crate::goal_tools::open_registry());
    tools.extend(
        crate::conversation_history::conversation_history_tool_registry()
            .into_iter()
            .filter(|tool| tool.name() == crate::conversation_attachment::READ_ATTACHMENT_TOOL),
    );
    tools.push(
        crate::ai_assistant::ai_assistant_provider_registry_ref()
            .capability_for_tool(crate::ai_assistant::PREVIEW_COMPUTER_ACTION_TOOL)
            .expect("registered pure preview")
            .registered_tool(),
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
    if ((ask.start_goal || ask.previous_completed_goal_id.is_some()) && ask.target_refs.len() != 1)
        || !ask.selected_capability_ids.is_empty()
        || (!ask.selected_attachment_ids.is_empty() && ask.target_refs.is_empty())
    {
        return Err("personal assistant input cannot carry device-bound context");
    }
    Ok(())
}

/// How a turn reaches a targetless conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetlessEntry {
    /// Owner input to the personal conversation.
    Input,
    /// Owner input appended or activated in a workbench work conversation.
    WorkInput,
    /// Continuation after a stored permission decision.
    PermissionResume,
    /// A delegated child's continuation after its own command result.
    ChildCompletion,
}

/// Which targetless conversation a turn belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetlessSubject {
    Personal,
    Work,
    Child,
}

fn classify_targetless(
    conversation_id: &str,
    entry: TargetlessEntry,
) -> Result<TargetlessSubject, &'static str> {
    use crate::native_ids::{Kind, is};
    let child = is(Kind::ChildConversation, conversation_id);
    match entry {
        TargetlessEntry::ChildCompletion if !child => {
            Err("child completion requires a delegated child conversation")
        }
        TargetlessEntry::PermissionResume | TargetlessEntry::ChildCompletion if child => {
            Ok(TargetlessSubject::Child)
        }
        TargetlessEntry::WorkInput | TargetlessEntry::PermissionResume
            if is(Kind::WorkbenchWorkConversation, conversation_id) =>
        {
            Ok(TargetlessSubject::Work)
        }
        _ => Ok(TargetlessSubject::Personal),
    }
}

/// The conversation is not the owner's personal conversation for this id.
pub const PERSONAL_KEY_MISMATCH: &str = "targetless input requires the personal conversation key";

/// The subject of a targetless turn, without its scope. A delegated child is
/// addressed by its native id; its task source is checked by the host. Every
/// other subject needs the owner's continuation id, and a personal one must
/// match the personal conversation key.
pub fn targetless_subject(
    actor_id: &str,
    conversation_id: &str,
    client_conversation_id: Option<&str>,
    entry: TargetlessEntry,
) -> Result<TargetlessSubject, &'static str> {
    let subject = classify_targetless(conversation_id, entry)?;
    if subject == TargetlessSubject::Child {
        return Ok(subject);
    }
    let continuation_id = client_conversation_id
        .filter(|id| is_valid_client_conversation_id(id))
        .ok_or("targetless input requires a valid continuation id")?;
    if subject == TargetlessSubject::Personal
        && conversation_id
            != derive_assistant_conversation_key(
                actor_id,
                AssistantConversationScope::Personal,
                continuation_id,
            )
    {
        return Err(PERSONAL_KEY_MISMATCH);
    }
    Ok(subject)
}

/// [`targetless_subject`] together with the targetless scope.
pub fn validate_targetless_turn(
    actor_id: &str,
    conversation_id: &str,
    client_conversation_id: Option<&str>,
    scope: &AgentScope,
    entry: TargetlessEntry,
) -> Result<TargetlessSubject, &'static str> {
    let subject = classify_targetless(conversation_id, entry)?;
    match subject {
        TargetlessSubject::Personal => {
            validate_targetless_input(actor_id, conversation_id, client_conversation_id, scope)?
        }
        TargetlessSubject::Work => {
            validate_personal_input_scope(client_conversation_id, scope)?;
        }
        TargetlessSubject::Child if !is_targetless_scope(scope) => {
            return Err("targetless input cannot carry device authority");
        }
        TargetlessSubject::Child => {}
    }
    Ok(subject)
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
    let registry = crate::ai_assistant::ai_assistant_provider_registry_ref();
    if let Some(target) = request.selected_device_target() {
        return crate::request_devices::accepts_device_reads(session)
            && session.admits_assistant_device(&target.device_id)
            && request.validate().is_ok()
            && request.items.iter().all(|item| {
                registry
                    .capability_for_tool(&item.tool_name)
                    .is_some_and(|capability| {
                        (crate::request_devices::selected_read_descriptor(capability)
                            || crate::request_devices::permits_selected_action(session, capability))
                            && capability.wire.effect == item.expected_effect
                            && registry
                                .provider_for_capability(&capability.wire.capability_id)
                                .is_some_and(|provider| {
                                    provider.wire.provider_id == item.provider_id
                                })
                    })
            });
    }
    session.has_admitted_assistant_input()
        && (request.exact_command().is_some_and(|command| {
            session
                .agent_role
                .allows_effect(crate::registry::ToolEffect::Mutating)
                && session.allows_new_mutation()
                && session.admits_assistant_device(&command.target_device_id)
                && command.actor_id == session.actor_id
                && command.input_revision == request.input_revision
                && command.policy_revision == session.policy_revision
        }) || (!request.items.is_empty()
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

/// A permission request with no device target: a central web read or search
/// asked by a personal assistant conversation, main or child. Owner and AI
/// review decide it alike; it carries no device authority.
pub fn is_targetless_permission(
    session: &PersistedAgentSession,
    request: &crate::dynamic_run::PermissionRequest,
) -> bool {
    crate::permission_grant::permission_device_id(session, request).is_empty()
        && !request.items.is_empty()
        && request
            .items
            .iter()
            .all(|item| crate::web_research::is_web_tool(&item.tool_name))
        && permits_permission_request(session, request)
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

/// A goal claim keeps the original conversation subject and its one frozen target.
pub fn validate_goal_claim(
    session: &PersistedAgentSession,
    goal: &crate::goal::GoalRun,
    params: &ClaimTurnParams,
) -> Result<(), AgentError> {
    require_current_policy(params.policy_revision)?;
    require_current_policy(session.policy_revision)?;
    if params.trigger_origin != TriggerOrigin::GoalContinuation
        || goal.owner_id != session.actor_id
        || goal.owner_id != params.actor_id
        || goal.conversation_id != session.conversation_id
        || goal.conversation_id != params.conversation_id
        || goal.device_id != session.device_id
        || goal.device_id != params.device_id
        || goal.input_revision != session.input_revision
        || session.single_assistant_request_target().ok() != Some(&goal.execution_target)
        || (params.device_id.is_empty() && !is_targetless_scope(&params.current_pdp_scope))
    {
        return Err(AgentError {
            kind: AgentErrorKind::SessionUnavailable,
            message: "Goal claim no longer matches its original request and device".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        });
    }
    if !params.device_id.is_empty() {
        validate_claim(session.surface, Some(session.policy_revision), params)?;
    }
    Ok(())
}

/// Whether the stored session and decision can authorize a personal resume,
/// independent of the caller's claim parameters. A device conversation is
/// authorized by its claim checks alone.
pub fn resume_decision_authorized(
    session: &PersistedAgentSession,
    decision: &crate::dynamic_run::PermissionDecidedEvent,
) -> bool {
    if !session.device_id.is_empty() {
        return true;
    }
    use crate::dynamic_run::PermissionDecisionSource;
    let decided_request = session.permission_requests.iter().find(|request| {
        request.request_id == decision.request_id
            && request.input_revision == decision.request_input_revision
            && request.state == decision.resulting_state
    });
    let selected_request = decided_request.filter(|request| {
        request.selected_device_id().is_some() && permits_permission_request(session, request)
    });
    // AI review decides a central web request exactly as the owner may.
    let reviewed_request = selected_request
        .or_else(|| decided_request.filter(|request| is_targetless_permission(session, request)));
    let authorized_decision = match &decision.decision_source {
        PermissionDecisionSource::UserDecision => true,
        PermissionDecisionSource::AutomaticReview { .. }
        | PermissionDecisionSource::SystemDecision { .. } => {
            reviewed_request.is_some() && session.allows_delegated_review()
        }
        PermissionDecisionSource::OwnerNoReview { authorization } => {
            selected_request.is_some_and(|request| request.exact_command().is_some())
                && session.has_direct_approval_origin()
                && authorization.root_conversation_id
                    == session.approval_delegation_conversation_id()
        }
    };
    session.has_admitted_personal_input()
        && (session.agent_role.is_main()
            || (session.agent_role.binding().is_some() && reviewed_request.is_some()))
        && session.input_revision == decision.request_input_revision
        && authorized_decision
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
    if resume_decision_authorized(session, decision)
        && params.trigger_origin == TriggerOrigin::PermissionDecision
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

/// A delegated child's claim after its own command result. A device child
/// keeps the ordinary claim check. A targetless child continues only its own
/// command result under the targetless scope; device authority still comes
/// from its request-bound grants. The host checks the pending trigger.
pub fn validate_child_work_claim(
    session: &PersistedAgentSession,
    params: &ClaimTurnParams,
) -> Result<(), AgentError> {
    if !params.device_id.is_empty() {
        return validate_claim(session.surface, Some(session.policy_revision), params);
    }
    require_current_policy(params.policy_revision)?;
    require_current_policy(session.policy_revision)?;
    if session.has_admitted_personal_input()
        && session.agent_role.binding().is_some()
        && matches!(
            params.trigger_origin,
            TriggerOrigin::ExecCompletion
                | TriggerOrigin::WorkCompletion {
                    kind: crate::session::WorkKind::AgentExec
                }
        )
        && is_targetless_scope(&params.current_pdp_scope)
    {
        return Ok(());
    }
    Err(AgentError {
        kind: AgentErrorKind::SessionUnavailable,
        message: "The delegated task is no longer eligible for command continuation".into(),
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

    fn targetless_scope() -> AgentScope {
        AgentScope {
            granted: vec![desk_agent_protocol::Capability::WebResearchFetch],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        }
    }

    #[test]
    fn a_targetless_turn_names_its_personal_work_or_child_subject() {
        use crate::native_ids::{Kind, stable};
        let personal = derive_assistant_conversation_key(
            "owner",
            AssistantConversationScope::Personal,
            "thread",
        );
        let work = stable(Kind::WorkbenchWorkConversation, &"work");
        let child = stable(Kind::ChildConversation, &"child");
        let subject = |conversation: &str, client: Option<&str>, entry| {
            targetless_subject("owner", conversation, client, entry)
        };
        use TargetlessEntry::*;
        assert_eq!(
            subject(&personal, Some("thread"), Input),
            Ok(TargetlessSubject::Personal)
        );
        assert!(subject(&personal, None, Input).is_err());
        assert!(subject("another", Some("thread"), PermissionResume).is_err());
        // Workbench work keeps requiring the owner's continuation id.
        assert_eq!(
            subject(&work, Some("thread"), WorkInput),
            Ok(TargetlessSubject::Work)
        );
        assert_eq!(
            subject(&work, Some("thread"), PermissionResume),
            Ok(TargetlessSubject::Work)
        );
        assert!(subject(&work, None, PermissionResume).is_err());
        assert!(subject(&work, Some("thread"), Input).is_err());
        // A delegated child has no continuation id, and only resumes.
        assert_eq!(
            subject(&child, None, PermissionResume),
            Ok(TargetlessSubject::Child)
        );
        assert_eq!(
            subject(&child, None, ChildCompletion),
            Ok(TargetlessSubject::Child)
        );
        assert!(subject(&child, None, Input).is_err());
        assert!(subject(&child, None, WorkInput).is_err());
        // The completion entry never admits a personal conversation.
        assert!(subject(&personal, Some("thread"), ChildCompletion).is_err());

        let scope = targetless_scope();
        assert_eq!(
            validate_targetless_turn("owner", &child, None, &scope, ChildCompletion),
            Ok(TargetlessSubject::Child)
        );
        let mut device_scope = scope.clone();
        device_scope.mode = ExecutionMode::ConfirmEachAction;
        assert!(
            validate_targetless_turn("owner", &child, None, &device_scope, PermissionResume)
                .is_err()
        );
        assert!(
            validate_targetless_turn("owner", &work, Some("thread"), &device_scope, WorkInput)
                .is_err()
        );
        assert_eq!(
            validate_targetless_turn("owner", &personal, Some("thread"), &scope, Input),
            Ok(TargetlessSubject::Personal)
        );
    }

    #[test]
    fn a_targetless_child_continues_only_its_own_command_result() {
        use crate::native_ids::{Kind, stable};
        use crate::subagent::{DelegatedTaskBinding, DelegationSource};
        let device = crate::dynamic_run::AssistantInputTarget::fixed("mac".into());
        let child = |device_id: &str| {
            PersistedAgentSession::new_subagent(
                stable(Kind::ChildConversation, &"child"),
                "owner",
                device_id,
                PERSONAL_ASSISTANT_POLICY_REVISION,
                targetless_scope(),
                DelegatedTaskBinding {
                    root_conversation_id: "root".into(),
                    group_id: "group".into(),
                    task_id: stable(Kind::Task, &"task"),
                    source: DelegationSource::UserInput { input_revision: 1 },
                    targets: vec![device.clone()],
                    objective: "Run the command".into(),
                    acceptance_criteria: vec!["Report the result".into()],
                    input_revision: 1,
                    control_revision: 1,
                    source_epoch: 1,
                    deadline_ms: 1_799_999_999_999,
                },
                "2026-11-11T00:00:00Z",
            )
            .unwrap()
        };
        let session = child("");
        let mut claim = ClaimTurnParams {
            conversation_id: session.conversation_id.clone(),
            actor_id: "owner".into(),
            device_id: String::new(),
            policy_revision: PERSONAL_ASSISTANT_POLICY_REVISION,
            current_pdp_scope: targetless_scope(),
            turn_id: "turn".into(),
            request_id: None,
            connection_id: None,
            now: "2026-11-11T00:00:00Z".into(),
            trigger_origin: TriggerOrigin::WorkCompletion {
                kind: crate::session::WorkKind::AgentExec,
            },
        };
        assert!(validate_child_work_claim(&session, &claim).is_ok());
        // The generic claim rule refuses this continuation.
        assert!(validate_claim(session.surface, Some(session.policy_revision), &claim).is_err());
        claim.trigger_origin = TriggerOrigin::ExecCompletion;
        assert!(validate_child_work_claim(&session, &claim).is_ok());
        for origin in [
            TriggerOrigin::User,
            TriggerOrigin::WorkContinuation,
            TriggerOrigin::WorkCompletion {
                kind: crate::session::WorkKind::ComputerAction,
            },
        ] {
            claim.trigger_origin = origin;
            assert!(validate_child_work_claim(&session, &claim).is_err());
        }
        claim.trigger_origin = TriggerOrigin::ExecCompletion;
        let mut device_scope = targetless_scope();
        device_scope.mode = ExecutionMode::ConfirmEachAction;
        claim.current_pdp_scope = device_scope;
        assert!(validate_child_work_claim(&session, &claim).is_err());
        claim.current_pdp_scope = targetless_scope();
        // A main conversation never takes the child rule.
        let mut main = session.clone();
        main.agent_role = crate::subagent::AgentRole::Main;
        assert!(validate_child_work_claim(&main, &claim).is_err());
        // A device child keeps the ordinary claim rule.
        let device_child = child("mac");
        claim.device_id = "mac".into();
        assert_eq!(
            validate_child_work_claim(&device_child, &claim).is_ok(),
            validate_claim(
                device_child.surface,
                Some(device_child.policy_revision),
                &claim
            )
            .is_ok()
        );
    }

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
    fn ai_reviewed_central_web_request_resumes_the_personal_conversation() {
        use crate::dynamic_run::{
            AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEvent, AgentRunEventKind,
            AiPermissionDecisionEvidence, PermissionDecidedEvent, PermissionDecisionSource,
            PermissionRequestState,
        };
        let registry = crate::ai_assistant::ai_assistant_provider_registry();
        let mut request = crate::permission_tools::build_permission_request(
            &crate::chat::ToolCall {
                id: "web-plan".into(),
                name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
                arguments_json: serde_json::json!({"items":[{
                    "item_id": "web", "tool_name": "fetch_public_web_page",
                    "exact_input": {"url": "https://example.com/releases"},
                    "suggested_ttl_seconds": 300, "suggested_max_uses": 1,
                    "reason": "Read the release notes",
                }]})
                .to_string(),
            },
            &registry,
            "web-permission".into(),
            1,
            "2026-10-09T00:00:00Z".into(),
        )
        .unwrap();
        request.state = PermissionRequestState::Approved;
        let scope = AgentScope {
            granted: vec![desk_agent_protocol::Capability::WebResearchFetch],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        };
        let mut session = PersistedAgentSession::new(
            "conversation",
            "owner",
            "",
            PERSONAL_ASSISTANT_POLICY_REVISION,
            scope.clone(),
            "2026-10-09T00:00:00Z",
        );
        session.surface = AgentSessionSurface::AiAssistant;
        session.input_revision = 1;
        session.trigger_origin = TriggerOrigin::PermissionDecision;
        session.bind_assistant_input_targets(&[]).unwrap();
        session.permission_requests.push(request.clone());
        assert!(is_targetless_permission(&session, &request));
        let decision = |source| PermissionDecidedEvent {
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: "decision".into(),
                run_id: "conversation".into(),
                event_seq: 2,
                input_revision: 1,
                kind: AgentRunEventKind::PermissionDecided,
                correlation_id: Some(request.request_id.clone()),
                source_envelope_ids: vec![],
                result_envelope_ids: vec![],
                created_at: "2026-10-09T00:00:01Z".into(),
            },
            request_id: request.request_id.clone(),
            request_input_revision: 1,
            resulting_state: PermissionRequestState::Approved,
            items: vec![],
            decision_source: source,
        };
        let params = ClaimTurnParams {
            conversation_id: "conversation".into(),
            actor_id: "owner".into(),
            device_id: String::new(),
            policy_revision: PERSONAL_ASSISTANT_POLICY_REVISION,
            current_pdp_scope: scope,
            turn_id: "resume".into(),
            request_id: None,
            connection_id: None,
            now: "2026-10-09T00:00:02Z".into(),
            trigger_origin: TriggerOrigin::PermissionDecision,
        };
        let reviewed = decision(PermissionDecisionSource::AutomaticReview {
            failures: vec![],
            delegation_id: "delegation".into(),
            delegation_revision: 1,
            reviews: vec![AiPermissionDecisionEvidence {
                item_id: "web".into(),
                candidate_id: "candidate".into(),
                candidate_expires_at_unix_ms: 1,
                model_config_revision: 1,
                reason_code: "within_scope".into(),
                reason: "The owner asked for this page".into(),
            }],
        });
        validate_permission_resume_claim(&session, &params, &reviewed).unwrap();
        validate_permission_resume_claim(
            &session,
            &params,
            &decision(PermissionDecisionSource::UserDecision),
        )
        .unwrap();
        // A child conversation requests central web reads like its main one.
        let mut child = session.clone();
        child.agent_role = crate::subagent::AgentRole::SubAgent {
            binding: Box::new(crate::subagent::DelegatedTaskBinding {
                targets: vec![],
                root_conversation_id: "root".into(),
                group_id: "group".into(),
                task_id: "task".into(),
                source: crate::subagent::DelegationSource::UserInput { input_revision: 1 },
                objective: "Read".into(),
                acceptance_criteria: vec!["Report".into()],
                input_revision: 1,
                control_revision: 1,
                source_epoch: 1,
                deadline_ms: 1,
            }),
        };
        assert!(is_targetless_permission(&child, &request));
        assert!(permits_permission_request(&child, &request));
        validate_permission_resume_claim(&child, &params, &reviewed).unwrap();
        assert!(resume_decision_authorized(&child, &reviewed));
        // Caller parameters do not change whether the stored decision authorizes.
        assert!(resume_decision_authorized(&session, &reviewed));
        let mut forged = params.clone();
        forged.policy_revision += 1;
        assert!(validate_permission_resume_claim(&session, &forged, &reviewed).is_err());
        let mut unreviewable = session.clone();
        unreviewable.trigger_origin = TriggerOrigin::ExecCompletion;
        assert!(!resume_decision_authorized(&unreviewable, &reviewed));
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
