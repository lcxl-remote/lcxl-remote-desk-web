//! Validation and projection of the platform goal budget.

use crate::goal::{
    DEFAULT_ACTIVE_TIME_MS, DEFAULT_DEADLINE_MS, DEFAULT_MODEL_CALLS, DEFAULT_MODEL_TOKENS,
    DEFAULT_SLICES, DEFAULT_STALLED_SLICES, DEFAULT_TOOL_CALLS, GoalError, GoalLimits,
};
use desk_agent_protocol::ai_assistant::goal_budget::{GoalBudgetLimits, GoalBudgetPolicy};

pub const SCHEMA_VERSION: u16 = 1;
const MAX_DEADLINE_MS: u64 = 30 * 24 * 60 * 60 * 1_000;
pub const DEFAULT_DEVICE_UNAVAILABLE_MAX_MS: u64 = 24 * 60 * 60 * 1_000;
pub const MIN_DEVICE_UNAVAILABLE_MAX_MS: u64 = 60 * 60 * 1_000;
pub const MAX_DEVICE_UNAVAILABLE_MAX_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

pub fn initial() -> GoalBudgetPolicy {
    GoalBudgetPolicy {
        schema_version: SCHEMA_VERSION,
        revision: 0,
        limits: GoalBudgetLimits {
            active_time_ms: Some(DEFAULT_ACTIVE_TIME_MS),
            deadline_ms: Some(DEFAULT_DEADLINE_MS),
            model_tokens: Some(DEFAULT_MODEL_TOKENS),
            model_calls: Some(DEFAULT_MODEL_CALLS),
            tool_calls: Some(DEFAULT_TOOL_CALLS),
            slices: Some(DEFAULT_SLICES),
            stalled_slices: Some(DEFAULT_STALLED_SLICES),
        },
        device_unavailable_max_ms: DEFAULT_DEVICE_UNAVAILABLE_MAX_MS,
    }
}

pub fn validate(policy: &GoalBudgetPolicy) -> Result<(), GoalError> {
    if policy.schema_version != SCHEMA_VERSION {
        return Err(GoalError::InvalidLimits);
    }
    validate_device_unavailable_max(policy.device_unavailable_max_ms)?;
    validate_limits(policy.limits)
}

pub fn validate_device_unavailable_max(value: u64) -> Result<(), GoalError> {
    if (MIN_DEVICE_UNAVAILABLE_MAX_MS..=MAX_DEVICE_UNAVAILABLE_MAX_MS).contains(&value) {
        Ok(())
    } else {
        Err(GoalError::InvalidLimits)
    }
}

pub fn validate_limits(value: GoalBudgetLimits) -> Result<(), GoalError> {
    let max = GoalLimits::policy_ceiling();
    let within = |number: Option<u64>, ceiling: u64| {
        number.is_none_or(|number| number > 0 && number <= ceiling)
    };
    if !within(value.active_time_ms, max.active_time_ms)
        || !within(value.deadline_ms, MAX_DEADLINE_MS)
        || !within(value.model_tokens, max.model_tokens)
        || !within(value.model_calls.map(u64::from), u64::from(max.model_calls))
        || !within(value.tool_calls.map(u64::from), u64::from(max.tool_calls))
        || !within(value.slices.map(u64::from), u64::from(max.slices))
        || !within(
            value.stalled_slices.map(u64::from),
            u64::from(max.stalled_slices),
        )
    {
        return Err(GoalError::InvalidLimits);
    }
    Ok(())
}

pub fn candidate(
    current: &GoalBudgetPolicy,
    limits: GoalBudgetLimits,
    device_unavailable_max_ms: u64,
) -> Result<GoalBudgetPolicy, GoalError> {
    validate(current)?;
    validate_limits(limits)?;
    validate_device_unavailable_max(device_unavailable_max_ms)?;
    Ok(GoalBudgetPolicy {
        schema_version: SCHEMA_VERSION,
        revision: current
            .revision
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?,
        limits,
        device_unavailable_max_ms,
    })
}

pub fn effective_limits(policy: &GoalBudgetPolicy) -> Result<GoalLimits, GoalError> {
    validate(policy)?;
    let value = policy.limits;
    Ok(GoalLimits {
        active_time_ms: value.active_time_ms.unwrap_or(u64::MAX),
        model_tokens: value.model_tokens.unwrap_or(u64::MAX),
        model_calls: value.model_calls.unwrap_or(u32::MAX),
        tool_calls: value.tool_calls.unwrap_or(u32::MAX),
        slices: value.slices.unwrap_or(u32::MAX),
        stalled_slices: value.stalled_slices.unwrap_or(u32::MAX),
    })
}

pub fn deadline_unix_ms(
    policy: &GoalBudgetPolicy,
    created_at_unix_ms: u64,
) -> Result<u64, GoalError> {
    validate(policy)?;
    policy
        .limits
        .deadline_ms
        .map_or(Ok(i64::MAX as u64), |duration| {
            created_at_unix_ms
                .checked_add(duration)
                .filter(|deadline| *deadline <= i64::MAX as u64)
                .ok_or(GoalError::ArithmeticOverflow)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_limits_remove_goal_budget_enforcement() {
        let initial = initial();
        let mut limits = initial.limits;
        limits.model_calls = None;
        limits.deadline_ms = None;
        let next = candidate(&initial, limits, DEFAULT_DEVICE_UNAVAILABLE_MAX_MS).unwrap();
        assert_eq!(next.revision, 1);
        assert_eq!(effective_limits(&next).unwrap().model_calls, u32::MAX);
        assert_eq!(deadline_unix_ms(&next, 1_000).unwrap(), i64::MAX as u64);
        limits.model_calls = Some(0);
        assert!(candidate(&next, limits, DEFAULT_DEVICE_UNAVAILABLE_MAX_MS).is_err());
    }

    #[test]
    fn device_unavailable_maximum_is_bounded_and_cannot_be_disabled() {
        let initial = initial();
        assert_eq!(initial.device_unavailable_max_ms, 86_400_000);
        for accepted in [3_600_000, 86_400_000, 2_592_000_000] {
            assert_eq!(
                candidate(&initial, initial.limits, accepted)
                    .unwrap()
                    .device_unavailable_max_ms,
                accepted
            );
        }
        for rejected in [0, 3_599_999, 2_592_000_001, u64::MAX] {
            assert!(candidate(&initial, initial.limits, rejected).is_err());
        }
        // Disabling the goal deadline leaves the device bound in force.
        let mut limits = initial.limits;
        limits.deadline_ms = None;
        let next = candidate(&initial, limits, 3_600_000).unwrap();
        assert_eq!(next.device_unavailable_max_ms, 3_600_000);
    }
}
