//! Validation and projection of the platform goal budget.

use crate::goal::{
    DEFAULT_ACTIVE_TIME_MS, DEFAULT_DEADLINE_MS, DEFAULT_MODEL_CALLS, DEFAULT_MODEL_TOKENS,
    DEFAULT_SLICES, DEFAULT_STALLED_SLICES, DEFAULT_TOOL_CALLS, GoalError, GoalLimits,
};
use desk_agent_protocol::ai_assistant::goal_budget::{GoalBudgetLimits, GoalBudgetPolicy};

pub const SCHEMA_VERSION: u16 = 1;
const MAX_DEADLINE_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

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
    }
}

pub fn validate(policy: &GoalBudgetPolicy) -> Result<(), GoalError> {
    if policy.schema_version != SCHEMA_VERSION {
        return Err(GoalError::InvalidLimits);
    }
    validate_limits(policy.limits)
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
) -> Result<GoalBudgetPolicy, GoalError> {
    validate(current)?;
    validate_limits(limits)?;
    Ok(GoalBudgetPolicy {
        schema_version: SCHEMA_VERSION,
        revision: current
            .revision
            .checked_add(1)
            .ok_or(GoalError::ArithmeticOverflow)?,
        limits,
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
        let next = candidate(&initial, limits).unwrap();
        assert_eq!(next.revision, 1);
        assert_eq!(effective_limits(&next).unwrap().model_calls, u32::MAX);
        assert_eq!(deadline_unix_ms(&next, 1_000).unwrap(), i64::MAX as u64);
        limits.model_calls = Some(0);
        assert!(candidate(&next, limits).is_err());
    }
}
