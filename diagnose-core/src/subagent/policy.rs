//! Admission limits count unfinished children across a main conversation.
use desk_agent_protocol::ai_assistant::subagent_policy::{SubAgentLimits, SubAgentPolicy};

pub const DEFAULT_UNFINISHED_PER_ROOT: u32 = 2;
pub const MAX_UNFINISHED_PER_ROOT: u32 = 32;

pub fn initial() -> SubAgentPolicy {
    SubAgentPolicy {
        schema_version: 1,
        revision: 0,
        limits: SubAgentLimits {
            max_unfinished_per_root: DEFAULT_UNFINISHED_PER_ROOT,
        },
    }
}

pub fn validate_limits(limits: SubAgentLimits) -> Result<(), &'static str> {
    if !(1..=MAX_UNFINISHED_PER_ROOT).contains(&limits.max_unfinished_per_root) {
        return Err("invalid subagent limits");
    }
    Ok(())
}

pub fn validate(policy: &SubAgentPolicy) -> Result<(), &'static str> {
    if policy.schema_version != 1 {
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
        schema_version: 1,
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
    fn the_single_unfinished_limit_has_explicit_bounds() {
        let current = initial();
        assert_eq!(current.limits.max_unfinished_per_root, 2);
        assert_eq!(
            serde_json::to_value(current.limits).unwrap(),
            serde_json::json!({"maxUnfinishedPerRoot": 2})
        );
        for limit in [1, 32] {
            assert_eq!(
                candidate(
                    &current,
                    SubAgentLimits {
                        max_unfinished_per_root: limit,
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
            validate(&SubAgentPolicy {
                schema_version: 2,
                ..current
            })
            .is_err()
        );
    }
}
