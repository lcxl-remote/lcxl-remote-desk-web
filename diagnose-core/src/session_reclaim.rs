//! Which idle AI Assistant sessions an expiry sweep may reclaim.
//!
//! Manager and the OSS signal server apply the same rule so the two products
//! cannot drift: work whose outcome is still being reconciled and any
//! non-terminal goal keep the originating session alive. A conversation timer
//! does not: it may only be created when it fires inside the session retention
//! window, and reclaiming the session cancels it in the same transaction.
//!
//! The rule reads the stored `agent_goal_run.status` column rather than the
//! decoded goal: the sweep only needs to know whether a goal has ended, and one
//! goal whose state no longer decodes must not stop reclaiming every session.

/// Stored `agent_goal_run.status` codes of goals that have ended. Every other
/// code, including one this build does not recognize, protects its session.
pub const TERMINAL_GOAL_STATUS_CODES: [&str; 3] = ["completed", "failed", "cancelled"];

/// Whether a goal with this stored status code still protects its session.
pub fn goal_status_protects_session(status_code: &str) -> bool {
    !TERMINAL_GOAL_STATUS_CODES.contains(&status_code)
}

/// Terminal state written to a conversation timer when its source session is
/// reclaimed, and the reason recorded with it.
pub const SOURCE_SESSION_EXPIRED: &str = "source_session_expired";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionReclaimBlocker {
    /// Dispatched or unreconciled work still needs its session as evidence.
    UnresolvedWork,
    /// A goal that has not reached a terminal state still owns the session.
    ActiveGoal,
}

/// Returns every reason the session must be kept, given whether it has
/// unresolved work and the stored status codes of its goals. An empty result
/// means the sweep may reclaim it (after its own age and turn-state checks).
pub fn session_reclaim_blockers<'a>(
    unresolved_work: bool,
    goal_status_codes: impl IntoIterator<Item = &'a str>,
) -> Vec<SessionReclaimBlocker> {
    let mut blockers = Vec::new();
    if unresolved_work {
        blockers.push(SessionReclaimBlocker::UnresolvedWork);
    }
    if goal_status_codes
        .into_iter()
        .any(goal_status_protects_session)
    {
        blockers.push(SessionReclaimBlocker::ActiveGoal);
    }
    blockers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::{GoalPauseReason, GoalWaitReason};

    #[test]
    fn only_unresolved_work_and_live_goals_protect_a_session() {
        assert!(session_reclaim_blockers(false, []).is_empty());
        assert!(session_reclaim_blockers(false, ["completed", "failed", "cancelled"]).is_empty());
        for code in ["queued", "running", "waiting_device", "paused", "blocked"] {
            assert_eq!(
                session_reclaim_blockers(false, ["completed", code]),
                vec![SessionReclaimBlocker::ActiveGoal]
            );
        }
        assert_eq!(
            session_reclaim_blockers(true, ["queued"]),
            vec![
                SessionReclaimBlocker::UnresolvedWork,
                SessionReclaimBlocker::ActiveGoal
            ]
        );
    }

    #[test]
    fn an_unrecognized_status_code_keeps_the_session() {
        assert_eq!(
            session_reclaim_blockers(false, ["cancelled", "some_future_state"]),
            vec![SessionReclaimBlocker::ActiveGoal]
        );
        assert!(goal_status_protects_session(""));
    }

    #[test]
    fn terminal_codes_are_exactly_the_terminal_status_codes() {
        use crate::goal::GoalState::*;
        let states = [
            Queued,
            Running,
            Waiting(GoalWaitReason::Approval),
            Waiting(GoalWaitReason::Work),
            Waiting(GoalWaitReason::Device),
            Waiting(GoalWaitReason::Model),
            Waiting(GoalWaitReason::User),
            Paused(GoalPauseReason::Owner),
            Blocked,
            Completed,
            Failed,
            Cancelled,
        ];
        let mut goal = crate::goal::GoalRun::new(
            "goal".into(),
            "run".into(),
            "owner".into(),
            "device".into(),
            "Finish".into(),
            "message".into(),
            crate::goal::GoalOpening::OwnerRequest,
            crate::goal::GoalModelBinding {
                connection_id: "gateway".into(),
                connection_revision: 1,
                profile_revision: 1,
                model_id: "model".into(),
            },
            1,
            1_000,
            crate::goal::GoalLimits::default(),
        )
        .unwrap();
        for state in states {
            goal.state = state;
            assert_eq!(
                goal_status_protects_session(goal.status_code()),
                !state.is_terminal(),
                "{state:?}"
            );
        }
    }
}
