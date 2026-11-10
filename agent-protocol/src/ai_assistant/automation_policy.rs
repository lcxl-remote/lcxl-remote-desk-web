//! Bound on automatic follow-up turns in one chain; not a permission grant.
//! Each dispatched action is still checked and approved on its own.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const SCHEMA_VERSION: u16 = 1;
/// Automatic turns one chain may spend before follow-ups stop for it.
pub const DEFAULT_MAX_AUTO_TURNS_PER_CHAIN: u32 = 20;
/// One keeps automation meaningful: a completion can fire one reaction.
pub const MIN_MAX_AUTO_TURNS_PER_CHAIN: u32 = 1;
/// A hard ceiling, so a misconfiguration cannot uncork an unbounded run.
pub const MAX_MAX_AUTO_TURNS_PER_CHAIN: u32 = 50;

pub fn valid_max_auto_turns_per_chain(value: u32) -> bool {
    (MIN_MAX_AUTO_TURNS_PER_CHAIN..=MAX_MAX_AUTO_TURNS_PER_CHAIN).contains(&value)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationPolicy {
    pub schema_version: u16,
    pub revision: u64,
    pub max_auto_turns_per_chain: u32,
}

impl Default for AutomationPolicy {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            revision: 0,
            max_auto_turns_per_chain: DEFAULT_MAX_AUTO_TURNS_PER_CHAIN,
        }
    }
}

impl AutomationPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported automation policy schema");
        }
        if !valid_max_auto_turns_per_chain(self.max_auto_turns_per_chain) {
            return Err("automatic turn limit is out of range");
        }
        Ok(())
    }

    /// The next revision with a new limit.
    pub fn candidate(&self, max_auto_turns_per_chain: u32) -> Result<Self, &'static str> {
        self.validate()?;
        let next = Self {
            schema_version: SCHEMA_VERSION,
            revision: self
                .revision
                .checked_add(1)
                .ok_or("automation policy revision exhausted")?,
            max_auto_turns_per_chain,
        };
        next.validate()?;
        Ok(next)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateAutomationPolicy {
    pub expected_revision: u64,
    pub max_auto_turns_per_chain: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_is_bounded_and_each_update_advances_the_revision() {
        let current = AutomationPolicy::default();
        assert_eq!(current.max_auto_turns_per_chain, 20);
        assert_eq!(
            serde_json::to_value(&current).unwrap(),
            serde_json::json!({"schemaVersion": 1, "revision": 0, "maxAutoTurnsPerChain": 20})
        );
        let next = current.candidate(5).unwrap();
        assert_eq!((next.revision, next.max_auto_turns_per_chain), (1, 5));
        assert!(current.candidate(0).is_err());
        assert!(current.candidate(51).is_err());
        assert!(current.candidate(50).is_ok());
    }
}
