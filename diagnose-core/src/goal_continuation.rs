//! Goal planning may overlap known background work, never an uncertain action.

use crate::goal::{GoalError, GoalRun};
use crate::session::{ActionIdentity, PersistedAgentSession};
use serde::{Deserialize, Serialize};

pub fn unresolved_work(session: &PersistedAgentSession) -> bool {
    session.execution_state.has_unresolved_outcome() || !session.unclosed_tool_call_ids().is_empty()
}

/// Ignore only the goal-control call currently being evaluated.
pub fn unresolved_work_except(session: &PersistedAgentSession, control_call_id: &str) -> bool {
    session.execution_state.has_unresolved_outcome()
        || session
            .unclosed_tool_call_ids()
            .iter()
            .any(|id| id != control_call_id)
}

pub fn pending_approval(session: &PersistedAgentSession) -> bool {
    session
        .permission_requests
        .iter()
        .any(|request| request.state == crate::dynamic_run::PermissionRequestState::Pending)
}

pub fn completion_pending(session: &PersistedAgentSession, control_call_id: &str) -> bool {
    unresolved_work_except(session, control_call_id)
        || !session.execution_state.tasks().is_empty()
        || pending_approval(session)
        || session.subagent_wait.is_some()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalClaimBinding {
    pub turn_id: String,
    pub segment: crate::focus_epoch::GoalSegmentIdentity,
    pub control_revision: u64,
    pub session_lease_token: u64,
}

impl GoalClaimBinding {
    pub fn from_session(session: &PersistedAgentSession) -> Result<Self, GoalError> {
        let binding = Self {
            turn_id: session
                .current_turn_id
                .clone()
                .ok_or(GoalError::InvalidIdentity)?,
            segment: session
                .focus_epoch
                .goal_segment
                .clone()
                .ok_or(GoalError::InvalidIdentity)?,
            control_revision: session.control_revision,
            session_lease_token: session.lease_token,
        };
        binding.validate()?;
        Ok(binding)
    }

    pub fn validate(&self) -> Result<(), GoalError> {
        if self.turn_id.trim().is_empty()
            || self.turn_id.len() > 256
            || self.segment.goal_id.is_empty()
            || self.segment.source_message_id.is_empty()
            || self.segment.segment_seq == 0
            || self.segment.goal_revision == 0
            || self.segment.lease_epoch == 0
            || self.control_revision == 0
            || self.session_lease_token == 0
        {
            return Err(GoalError::InvalidIdentity);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalWorkWait {
    pub wait_id: String,
    pub goal_id: String,
    pub input_revision: u64,
    pub goal_revision: u64,
    pub control_revision: u64,
    pub members: Vec<ActionIdentity>,
    pub ready: bool,
    pub ready_event_ids: Vec<String>,
}

impl GoalWorkWait {
    pub fn new(
        goal: &GoalRun,
        session: &PersistedAgentSession,
        call_id: &str,
        members: Vec<ActionIdentity>,
    ) -> Result<Self, GoalError> {
        let wait = Self {
            wait_id: crate::native_ids::stable(
                crate::native_ids::Kind::Lineage,
                &(
                    &goal.goal_id,
                    goal.slice_seq,
                    goal.lease_epoch,
                    session.input_revision,
                    session.control_revision,
                    call_id,
                ),
            ),
            goal_id: goal.goal_id.clone(),
            input_revision: session.input_revision,
            goal_revision: goal.goal_revision,
            control_revision: session.control_revision,
            members,
            ready: false,
            ready_event_ids: Vec::new(),
        };
        wait.validate()?;
        Ok(wait)
    }

    pub fn validate(&self) -> Result<(), GoalError> {
        if self.wait_id.trim().is_empty()
            || self.wait_id.len() > 256
            || self.goal_id.trim().is_empty()
            || self.input_revision == 0
            || self.goal_revision == 0
            || self.control_revision == 0
            || self.ready == self.ready_event_ids.is_empty()
            || self.ready_event_ids.len() > self.members.len()
            || self.ready_event_ids.iter().enumerate().any(|(index, id)| {
                id.is_empty() || id.len() > 256 || self.ready_event_ids[..index].contains(id)
            })
            || self.members.is_empty()
            || self.members.len() > crate::session::MAX_PENDING_AUTO_TRIGGERS
        {
            return Err(GoalError::InvalidIdentity);
        }
        for (index, member) in self.members.iter().enumerate() {
            if member.work_id <= 0
                || member.execution_id.trim().is_empty()
                || member.action_request_id.trim().is_empty()
                || self.members[..index].iter().any(|previous| {
                    (previous.kind == member.kind && previous.work_id == member.work_id)
                        || previous.execution_id == member.execution_id
                        || previous.action_request_id == member.action_request_id
                })
            {
                return Err(GoalError::InvalidIdentity);
            }
        }
        Ok(())
    }

    pub fn matches(&self, goal: &GoalRun, control_revision: u64) -> bool {
        self.goal_id == goal.goal_id
            && self.input_revision == goal.input_revision
            && self.goal_revision == goal.goal_revision
            && self.control_revision == control_revision
            && self.validate().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{BackgroundExecution, ExecutionState, UnknownExecution};

    fn session() -> PersistedAgentSession {
        PersistedAgentSession::new(
            "run",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "2026-10-15T00:00:00Z",
        )
    }

    #[test]
    fn known_concurrent_work_allows_planning_but_not_completion() {
        let mut session = session();
        session.execution_state = ExecutionState::from_states(vec![
            ExecutionState::Executing {
                action: ActionIdentity::agent_exec(1, "a", "generation-a"),
            },
            ExecutionState::Executing {
                action: ActionIdentity::agent_exec(2, "b", "generation-b"),
            },
        ]);
        assert!(!unresolved_work(&session));
        assert!(completion_pending(&session, "control"));
    }

    #[test]
    fn unknown_members_and_unbound_interruptions_remain_unresolved() {
        let mut session = session();
        session.execution_state = ExecutionState::Concurrent {
            tasks: vec![BackgroundExecution {
                action: ActionIdentity::agent_exec(1, "a", "generation-a"),
                unknown: Some(UnknownExecution {
                    placeholder_message_id: "placeholder".into(),
                    since: "now".into(),
                }),
            }],
            interrupted_since: None,
        };
        assert!(unresolved_work(&session));
        session.execution_state = ExecutionState::Concurrent {
            tasks: vec![],
            interrupted_since: Some("now".into()),
        };
        assert!(unresolved_work(&session));
    }

    fn running_goal() -> GoalRun {
        let mut goal = GoalRun::new(
            "goal".into(),
            "run".into(),
            "owner".into(),
            "device".into(),
            "Complete work".into(),
            "input".into(),
            crate::goal::GoalOpening::OwnerRequest,
            crate::goal::GoalModelBinding {
                connection_id: "gateway".into(),
                connection_revision: 1,
                profile_revision: 1,
                model_id: "model".into(),
            },
            1,
            1000,
            crate::goal::GoalLimits::default(),
        )
        .unwrap();
        goal.claim_slice(1001).unwrap();
        goal
    }

    #[test]
    fn wait_is_separate_from_replaced_checkpoints_and_rejects_duplicate_tasks() {
        let mut session = session();
        session.input_revision = 1;
        let mut goal = running_goal();
        let action = ActionIdentity::agent_exec(1, "task", "generation");
        assert!(
            GoalWorkWait::new(
                &goal,
                &session,
                "call",
                vec![action.clone(), action.clone()]
            )
            .is_err()
        );
        let wait = GoalWorkWait::new(&goal, &session, "call", vec![action]).unwrap();
        goal.finish_slice(
            1,
            1,
            1,
            &crate::goal::GoalControl::WaitForWork {
                wait: wait.clone(),
                message: "Waiting for native results".into(),
            },
            false,
            false,
            1,
            vec![],
            1002,
        )
        .unwrap();
        assert_eq!(
            goal.state,
            crate::goal::GoalState::Waiting(crate::goal::GoalWaitReason::Work)
        );
        goal.wake_from(crate::goal::GoalWaitReason::Work, 1003)
            .unwrap();
        goal.claim_slice(1004).unwrap();
        goal.finish_slice(
            1,
            1,
            2,
            &crate::goal::GoalControl::Continue {
                progress: "Processed a result".into(),
                next_step: "Continue".into(),
            },
            true,
            false,
            2,
            vec![],
            1005,
        )
        .unwrap();
        assert_eq!(goal.work_wait, Some(wait));
        let restored: GoalRun =
            serde_json::from_str(&serde_json::to_string(&goal).unwrap()).unwrap();
        restored.validate().unwrap();
        assert_eq!(restored, goal);
    }

    #[test]
    fn claim_records_the_actual_turn_without_relying_on_its_spelling() {
        let mut session = session();
        session.input_revision = 1;
        session.focus_epoch.input_revision = 1;
        session.surface = crate::session::AgentSessionSurface::AiAssistant;
        session
            .begin_goal_segment("goal", "input", 1, 1, 1)
            .unwrap();
        session
            .begin_turn(
                "ordinary-random-turn",
                None,
                None,
                1,
                session.scope_snapshot.clone(),
                "2026-10-15T00:00:01Z",
            )
            .unwrap();
        let goal = running_goal();
        let mut event = crate::goal::GoalLedgerEvent::new(
            &goal,
            crate::dynamic_run::AgentRunEventKind::GoalSliceClaimed,
            1,
            "2026-10-15T00:00:01Z".into(),
        )
        .unwrap();
        event.bind_claim(&goal, &session).unwrap();
        assert_eq!(
            event.claim.as_ref().unwrap().turn_id,
            "ordinary-random-turn"
        );
        assert_eq!(event.claim.as_ref().unwrap().segment.goal_id, "goal");
    }
}
