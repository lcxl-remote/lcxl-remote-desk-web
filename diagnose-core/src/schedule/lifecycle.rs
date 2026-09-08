//! Shared failure accounting; callers persist each transition under a DB fence.

use desk_agent_protocol::schedule::{SchedulePauseReason, ScheduledRunStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureState {
    pub recovery_epoch: u64,
    pub consecutive_failures: u32,
    pub failure_threshold: u32,
    pub pause_reasons: BTreeSet<SchedulePauseReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureTransitionError {
    InvalidThreshold,
    Nonterminal,
    Overflow,
    BlockingReasons,
}

impl Default for FailureState {
    fn default() -> Self {
        Self {
            recovery_epoch: 1,
            consecutive_failures: 0,
            failure_threshold: 3,
            pause_reasons: BTreeSet::new(),
        }
    }
}

impl FailureState {
    /// Threshold changes never erase a pause or reset the observed failure count.
    pub fn set_threshold(&mut self, threshold: u32) -> Result<(), FailureTransitionError> {
        if threshold == 0 {
            return Err(FailureTransitionError::InvalidThreshold);
        }
        self.failure_threshold = threshold;
        if self.consecutive_failures >= threshold {
            self.pause_reasons
                .insert(SchedulePauseReason::ConsecutiveFailures);
        }
        Ok(())
    }

    /// Duplicate finalization must be excluded by the persisted run's accounted flag.
    /// A late receipt may refine history but never alters the current recovery epoch.
    pub fn settle(
        &mut self,
        run_epoch: u64,
        status: ScheduledRunStatus,
        offline_timeout: bool,
    ) -> Result<bool, FailureTransitionError> {
        if self.failure_threshold == 0 {
            return Err(FailureTransitionError::InvalidThreshold);
        }
        if !status.is_terminal() {
            return Err(FailureTransitionError::Nonterminal);
        }
        if run_epoch != self.recovery_epoch {
            return Ok(false);
        }
        match status {
            ScheduledRunStatus::Succeeded => self.consecutive_failures = 0,
            ScheduledRunStatus::Failed => self.record_failure()?,
            ScheduledRunStatus::Missed if offline_timeout => self.record_failure()?,
            ScheduledRunStatus::OutcomeUnknown => {
                self.pause_reasons
                    .insert(SchedulePauseReason::UnknownSideEffect);
            }
            _ => {}
        }
        Ok(true)
    }

    fn record_failure(&mut self) -> Result<(), FailureTransitionError> {
        self.consecutive_failures = self
            .consecutive_failures
            .checked_add(1)
            .ok_or(FailureTransitionError::Overflow)?;
        if self.consecutive_failures >= self.failure_threshold {
            self.pause_reasons
                .insert(SchedulePauseReason::ConsecutiveFailures);
        }
        Ok(())
    }

    /// Resolution of authorization/unknown-result blockers happens separately after
    /// evidence checks. An ordinary resume can clear only user/threshold pauses.
    pub fn resume(&mut self) -> Result<(), FailureTransitionError> {
        if self.pause_reasons.iter().any(|r| {
            !matches!(
                r,
                SchedulePauseReason::User | SchedulePauseReason::ConsecutiveFailures
            )
        }) {
            return Err(FailureTransitionError::BlockingReasons);
        }
        let epoch = self
            .recovery_epoch
            .checked_add(1)
            .ok_or(FailureTransitionError::Overflow)?;
        self.recovery_epoch = epoch;
        self.consecutive_failures = 0;
        self.pause_reasons.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failure_threshold_survives_skips_and_success_resets_only_count() {
        let mut state = FailureState::default();
        state.settle(1, ScheduledRunStatus::Failed, false).unwrap();
        state
            .settle(1, ScheduledRunStatus::SkippedOverlap, false)
            .unwrap();
        state.settle(1, ScheduledRunStatus::Missed, false).unwrap();
        state
            .settle(1, ScheduledRunStatus::Cancelled, false)
            .unwrap();
        assert_eq!(state.consecutive_failures, 1);
        state.settle(1, ScheduledRunStatus::Missed, true).unwrap();
        state.settle(1, ScheduledRunStatus::Failed, false).unwrap();
        assert!(
            state
                .pause_reasons
                .contains(&SchedulePauseReason::ConsecutiveFailures)
        );
        state
            .settle(1, ScheduledRunStatus::Succeeded, false)
            .unwrap();
        assert_eq!(state.consecutive_failures, 0);
        assert!(!state.pause_reasons.is_empty());
    }

    #[test]
    fn unknown_and_authorization_blockers_cannot_be_cleared_by_resume() {
        let mut state = FailureState::default();
        state
            .settle(1, ScheduledRunStatus::OutcomeUnknown, false)
            .unwrap();
        state
            .pause_reasons
            .insert(SchedulePauseReason::AuthorizationInvalid);
        assert_eq!(state.resume(), Err(FailureTransitionError::BlockingReasons));
        state
            .pause_reasons
            .remove(&SchedulePauseReason::UnknownSideEffect);
        assert_eq!(state.resume(), Err(FailureTransitionError::BlockingReasons));
    }

    #[test]
    fn recovery_excludes_old_receipts_and_rejects_nonterminal_accounting() {
        let mut state = FailureState::default();
        state.settle(1, ScheduledRunStatus::Failed, false).unwrap();
        state.resume().unwrap();
        state.settle(2, ScheduledRunStatus::Failed, false).unwrap();
        assert_eq!(
            state.settle(1, ScheduledRunStatus::Succeeded, false),
            Ok(false)
        );
        assert_eq!(
            state.settle(2, ScheduledRunStatus::Running, false),
            Err(FailureTransitionError::Nonterminal)
        );
        assert_eq!(state.consecutive_failures, 1);
    }
}
