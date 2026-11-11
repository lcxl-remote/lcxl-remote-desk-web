//! Session checks a host runs when a delegated child claims the turn that
//! continues its own command result. Hosts keep their transaction, locks and
//! stored-row lookups; the order is: [`ready`], the host's child-source checks,
//! the model destination and [`has_pending_exec`], then
//! [`crate::assistant_policy::validate_child_work_claim`].
use crate::{
    seam::ClaimTurnParams,
    session::{PersistedAgentSession, WorkKind},
};

/// The stored child can take a completion turn now.
pub fn ready(
    session: &PersistedAgentSession,
    stored_version: i64,
    stored_lease_token: i64,
    params: &ClaimTurnParams,
    auto_turn_limit: u32,
) -> bool {
    session.agent_role.binding().is_some()
        && !session.turn_state.is_active()
        && session.version == stored_version
        && i64::try_from(session.lease_token).ok() == Some(stored_lease_token)
        && session.scope_snapshot == params.current_pdp_scope
        && session.automation_turns_used < auto_turn_limit
}

/// The child still waits for this exact command result in its current chain.
pub fn has_pending_exec(session: &PersistedAgentSession, event_id: &str) -> bool {
    session.pending_auto_triggers.iter().any(|pending| {
        pending.event_id == event_id
            && pending.kind == WorkKind::AgentExec
            && pending.chain_id == session.chain_id
            && pending.resolution_org_id.is_none()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentScope, ExecutionMode};

    fn child() -> PersistedAgentSession {
        use crate::native_ids::{Kind, stable};
        PersistedAgentSession::new_subagent(
            stable(Kind::ChildConversation, &"child"),
            "owner",
            "",
            1,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            crate::subagent::DelegatedTaskBinding {
                root_conversation_id: "root".into(),
                group_id: "group".into(),
                task_id: stable(Kind::Task, &"task"),
                source: crate::subagent::DelegationSource::UserInput { input_revision: 1 },
                targets: vec![crate::dynamic_run::AssistantInputTarget::fixed(
                    "mac".into(),
                )],
                objective: "Run the command".into(),
                acceptance_criteria: vec!["Report".into()],
                input_revision: 1,
                control_revision: 1,
                source_epoch: 1,
                deadline_ms: 1_799_999_999_999,
            },
            "2026-11-11T00:00:00Z",
        )
        .unwrap()
    }

    fn claim(session: &PersistedAgentSession) -> ClaimTurnParams {
        ClaimTurnParams {
            conversation_id: session.conversation_id.clone(),
            actor_id: "owner".into(),
            device_id: String::new(),
            policy_revision: 1,
            current_pdp_scope: session.scope_snapshot.clone(),
            turn_id: "turn".into(),
            request_id: None,
            connection_id: None,
            now: "2026-11-11T00:00:00Z".into(),
            trigger_origin: crate::session::TriggerOrigin::WorkCompletion {
                kind: WorkKind::AgentExec,
            },
        }
    }

    #[test]
    fn a_child_is_ready_only_while_idle_current_and_within_budget() {
        let mut session = child();
        let params = claim(&session);
        let (version, lease) = (session.version, session.lease_token as i64);
        assert!(ready(&session, version, lease, &params, 3));
        assert!(!ready(&session, version + 1, lease, &params, 3));
        assert!(!ready(&session, version, lease + 1, &params, 3));
        assert!(!ready(&session, version, lease, &params, 0));
        let mut changed = params.clone();
        changed.current_pdp_scope.mode = ExecutionMode::ConfirmEachAction;
        assert!(!ready(&session, version, lease, &changed, 3));
        let mut main = session.clone();
        main.agent_role = crate::subagent::AgentRole::Main;
        assert!(!ready(&main, version, lease, &params, 3));
        session
            .begin_turn("t", None, None, 1, session.scope_snapshot.clone(), "now")
            .unwrap();
        assert!(!ready(&session, version, lease, &params, 3));
    }

    #[test]
    fn a_child_owns_actions_on_its_admitted_devices_and_others_only_their_own() {
        let session = child();
        assert!(session.owns_action_device("mac"));
        assert!(!session.owns_action_device("linux"));
        assert!(!session.owns_action_device(""));
        // Without a delegated binding, only the session's own device counts,
        // even when its request admits other devices.
        let mut main = session.clone();
        main.agent_role = crate::subagent::AgentRole::Main;
        assert!(main.admits_assistant_device("mac"));
        assert!(!main.owns_action_device("mac"));
        assert!(main.owns_action_device(""));
    }

    #[test]
    fn only_the_exact_pending_command_result_continues_the_child() {
        let mut session = child();
        let pending = |event: &str, kind, chain: &str, org| crate::session::PendingAutoTrigger {
            work_id: 1,
            kind,
            execution_id: "exec".into(),
            tool_call_id: "call".into(),
            event_id: event.into(),
            chain_id: chain.into(),
            resolution_org_id: org,
            since: "2026-11-11T00:00:00Z".into(),
        };
        let chain = session.chain_id.clone();
        session
            .pending_auto_triggers
            .push(pending("done", WorkKind::AgentExec, &chain, None));
        assert!(has_pending_exec(&session, "done"));
        assert!(!has_pending_exec(&session, "other"));
        for stale in [
            pending("ui", WorkKind::ComputerAction, &chain, None),
            pending("old", WorkKind::AgentExec, "old-chain", None),
            pending("org", WorkKind::AgentExec, &chain, Some(1)),
        ] {
            let event = stale.event_id.clone();
            session.pending_auto_triggers.push(stale);
            assert!(!has_pending_exec(&session, &event));
        }
    }
}
