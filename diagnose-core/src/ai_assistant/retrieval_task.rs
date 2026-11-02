//! Original task-state bindings for source reads. These are neither execution
//! permission nor substitutes for native owner/session/task database fences.

use crate::{
    goal::{GoalRun, GoalState},
    session::{PersistedAgentSession, TriggerOrigin},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalRetrievalAuthority {
    pub expires_at_unix_ms: u64,
    /// A background segment needs its original live native planner lease.
    pub requires_segment_lease: bool,
}

impl GoalRetrievalAuthority {
    pub fn native_deadline(
        self,
        lease_owner: Option<&str>,
        lease_deadline: Option<i64>,
        now: u64,
    ) -> Result<u64, &'static str> {
        if now == 0 || now > i64::MAX as u64 || self.expires_at_unix_ms <= now {
            return Err("original retrieval goal expired");
        }
        if self.requires_segment_lease {
            let deadline = lease_deadline
                .filter(|until| *until > now as i64)
                .ok_or("retrieval goal segment lease expired")? as u64;
            if lease_owner.is_none_or(|owner| owner.is_empty()) {
                return Err("retrieval goal segment has no lease owner");
            }
            Ok(self.expires_at_unix_ms.min(deadline))
        } else if lease_owner.is_some() || lease_deadline.is_some() {
            Err("interactive retrieval goal still has a background lease")
        } else {
            Ok(self.expires_at_unix_ms)
        }
    }
}

pub fn goal_authority(
    session: &PersistedAgentSession,
    goal: &GoalRun,
    now: u64,
) -> Result<GoalRetrievalAuthority, &'static str> {
    goal.validate()
        .map_err(|_| "invalid native retrieval goal")?;
    let data = session
        .assistant_data
        .as_ref()
        .ok_or("missing original task retrieval state")?;
    if !super::data_access::eligible_origin(session)
        || now == 0
        || now > i64::MAX as u64
        || data.retrieval_task_id.as_deref() != Some(goal.goal_id.as_str())
        || goal.conversation_id != session.conversation_id
        || goal.owner_id != session.actor_id
        || goal.device_id != session.device_id
        || goal.input_revision != session.input_revision
        || goal.deadline_unix_ms <= now
        || goal.state.is_terminal()
    {
        return Err("retrieval goal no longer matches its original current input");
    }
    let requires_segment_lease = if let Some(segment) = &session.focus_epoch.goal_segment {
        if !matches!(
            session.trigger_origin,
            TriggerOrigin::GoalContinuation | TriggerOrigin::PermissionDecision
        ) || segment.goal_id != goal.goal_id
            || segment.segment_seq != goal.slice_seq
            || segment.source_message_id != goal.source_message_id
            || goal
                .require_fence(
                    session.input_revision,
                    segment.goal_revision,
                    segment.lease_epoch,
                )
                .is_err()
        {
            return Err("retrieval goal segment is stale");
        }
        true
    } else {
        // An explicit current owner reply can inspect history while clarification
        // or recovery is pending. It never resumes the background planner.
        if !matches!(
            session.trigger_origin,
            TriggerOrigin::User
                | TriggerOrigin::PermissionDecision
                | TriggerOrigin::WorkContinuation
        ) || goal.state == GoalState::Running
        {
            return Err("retrieval goal requires its original claimed segment");
        }
        false
    };
    Ok(GoalRetrievalAuthority {
        expires_at_unix_ms: goal.deadline_unix_ms,
        requires_segment_lease,
    })
}

#[cfg(test)]
mod tests;
