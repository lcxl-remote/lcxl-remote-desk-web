//! Admission limits count unfinished children across a main conversation.
use desk_agent_protocol::ai_assistant::subagent_policy::{SubAgentLimits, SubAgentPolicy};

pub const DEFAULT_UNFINISHED_PER_ROOT: u32 = 2;
pub const MAX_UNFINISHED_PER_ROOT: u32 = 32;
pub const DEFAULT_TASK_DURATION_MINUTES: u32 = 120;
const SCHEMA_VERSION: u16 = 2;

pub fn initial() -> SubAgentPolicy {
    SubAgentPolicy {
        schema_version: SCHEMA_VERSION,
        revision: 0,
        limits: SubAgentLimits {
            max_unfinished_per_root: DEFAULT_UNFINISHED_PER_ROOT,
            max_task_duration_minutes: DEFAULT_TASK_DURATION_MINUTES,
        },
    }
}

pub fn validate_limits(limits: SubAgentLimits) -> Result<(), &'static str> {
    if !(1..=MAX_UNFINISHED_PER_ROOT).contains(&limits.max_unfinished_per_root)
        || limits.max_task_duration_minutes == 0
    {
        return Err("invalid subagent limits");
    }
    Ok(())
}

pub fn validate(policy: &SubAgentPolicy) -> Result<(), &'static str> {
    if policy.schema_version != SCHEMA_VERSION {
        return Err("invalid subagent policy version");
    }
    validate_limits(policy.limits)
}

pub fn candidate(
    current: &SubAgentPolicy,
    limits: SubAgentLimits,
) -> Result<SubAgentPolicy, &'static str> {
    validate(current)?;
    validate_limits(limits)?;
    Ok(SubAgentPolicy {
        schema_version: SCHEMA_VERSION,
        revision: current
            .revision
            .checked_add(1)
            .ok_or("subagent policy revision exhausted")?,
        limits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_limits_require_positive_integers() {
        let current = initial();
        assert_eq!(current.limits.max_unfinished_per_root, 2);
        assert_eq!(
            serde_json::to_value(current.limits).unwrap(),
            serde_json::json!({"maxUnfinishedPerRoot": 2, "maxTaskDurationMinutes": 120})
        );
        for limit in [1, 32] {
            assert_eq!(
                candidate(
                    &current,
                    SubAgentLimits {
                        max_unfinished_per_root: limit,
                        ..current.limits
                    }
                )
                .unwrap()
                .revision,
                1
            );
        }
        for limit in [0, 33] {
            assert!(
                candidate(
                    &current,
                    SubAgentLimits {
                        max_unfinished_per_root: limit,
                        ..current.limits
                    }
                )
                .is_err()
            );
        }
        assert!(
            candidate(
                &SubAgentPolicy {
                    revision: u64::MAX,
                    ..current.clone()
                },
                current.limits
            )
            .is_err()
        );
        assert!(
            candidate(
                &current,
                SubAgentLimits {
                    max_task_duration_minutes: 0,
                    ..current.limits
                }
            )
            .is_err()
        );
        assert!(
            validate(&SubAgentPolicy {
                schema_version: 1,
                ..current
            })
            .is_err()
        );
    }
}
