//! Which idle AI Assistant sessions an expiry sweep may reclaim.
//!
//! Manager and the OSS signal server apply the same rule so the two products
//! cannot drift: work whose outcome is still being reconciled and any
//! non-terminal goal keep the originating session alive. A conversation timer
//! does not: it may only be created when it fires inside the session retention
//! window, and reclaiming the session cancels it in the same transaction.

use crate::goal::GoalState;

/// Stored `agent_goal_run.status` codes of goals that still protect their
/// session. Terminal codes (`completed`, `failed`, `cancelled`) are absent.
pub const PROTECTING_GOAL_STATUS_CODES: [&str; 9] = [
    "queued",
    "running",
    "waiting_approval",
    "waiting_work",
    "waiting_device",
    "waiting_model",
    "waiting_user",
    "paused",
    "blocked",
];

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

/// Returns every reason the session must be kept. An empty result means the
/// sweep may reclaim it (after its own age and turn-state checks).
pub fn session_reclaim_blockers(
    unresolved_work: bool,
    goal_states: impl IntoIterator<Item = GoalState>,
) -> Vec<SessionReclaimBlocker> {
    let mut blockers = Vec::new();
    if unresolved_work {
        blockers.push(SessionReclaimBlocker::UnresolvedWork);
    }
    if goal_states.into_iter().any(|state| !state.is_terminal()) {
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
        assert!(
            session_reclaim_blockers(
                false,
                [
                    GoalState::Completed,
                    GoalState::Failed,
                    GoalState::Cancelled
                ]
            )
            .is_empty()
        );
        for state in [
            GoalState::Queued,
            GoalState::Running,
            GoalState::Waiting(GoalWaitReason::Device),
            GoalState::Paused(GoalPauseReason::Owner),
            GoalState::Blocked,
        ] {
            assert_eq!(
                session_reclaim_blockers(false, [GoalState::Completed, state]),
                vec![SessionReclaimBlocker::ActiveGoal]
            );
        }
        assert_eq!(
            session_reclaim_blockers(true, [GoalState::Queued]),
            vec![
                SessionReclaimBlocker::UnresolvedWork,
                SessionReclaimBlocker::ActiveGoal
            ]
        );
    }

    #[test]
    fn protecting_codes_are_exactly_the_non_terminal_status_codes() {
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
                PROTECTING_GOAL_STATUS_CODES.contains(&goal.status_code()),
                !state.is_terminal(),
                "{state:?}"
            );
        }
    }
}
